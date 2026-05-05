//! `axum`-backed HTTP edge.
//!
//! Routes (SPEC §3.3):
//!
//! ```text
//! /wms        WMS 1.3.0
//! /healthz    liveness
//! /readyz     readiness (gated on a usable manifest)
//! /metrics    Prometheus scrape
//! ```
//!
//! WMTS lands in Phase 1 alongside the tile-cache.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mars_observability::Metrics;
use mars_runtime::{RenderPlan, Runtime, RuntimeError};
use mars_wms::{WmsConfig, WmsError, WmsRequest};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing::Instrument;

const BODY_LIMIT_BYTES: usize = 1 << 20; // 1 MiB
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("listen error: {0}")]
    Listen(String),
}

/// HTTP server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub debug_endpoints: bool,
}

/// Atomically swappable capabilities document. Cheap clone, lock-free reads.
pub type CapabilitiesHandle = Arc<ArcSwap<String>>;

/// Helper to build a fresh [`CapabilitiesHandle`] seeded with `body`.
#[must_use]
pub fn capabilities_handle(body: String) -> CapabilitiesHandle {
    Arc::new(ArcSwap::from(Arc::new(body)))
}

/// Shared per-request state.
#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesHandle,
    wms_cfg: Arc<WmsConfig>,
    metrics: Metrics,
    request_counter: Arc<AtomicU64>,
}

/// Build the router. Exposed for in-process testing via `tower::ServiceExt`.
pub fn router(runtime: Arc<Runtime>, capabilities: CapabilitiesHandle, wms_cfg: WmsConfig, metrics: Metrics) -> Router {
    let state = AppState {
        runtime,
        capabilities,
        wms_cfg: Arc::new(wms_cfg),
        metrics,
        request_counter: Arc::new(AtomicU64::new(0)),
    };
    Router::new()
        .route("/wms", get(handle_wms))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/readyz", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, observe_request))
        .layer(RequestBodyLimitLayer::new(BODY_LIMIT_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
}

/// Run the HTTP server until ctrl_c.
pub async fn serve(
    cfg: ServerConfig,
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesHandle,
    wms_cfg: WmsConfig,
    metrics: Metrics,
) -> Result<(), HttpError> {
    let app = router(runtime, capabilities, wms_cfg, metrics);
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| HttpError::Listen(format!("bind {}: {e}", cfg.listen)))?;
    tracing::info!(addr = %cfg.listen, "http: listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| HttpError::Listen(e.to_string()))
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(%e, "ctrl_c handler failed");
    }
    tracing::info!("http: shutdown requested");
}

// ---------- middleware ----------

async fn observe_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let interface = interface_label(req.uri().path());
    let start = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    state.metrics.observe_request(interface, status, start.elapsed());
    resp
}

fn interface_label(path: &str) -> &'static str {
    // strict prefix match; anything outside the known set is bucketed as "other"
    // to keep cardinality flat regardless of probing/garbage requests.
    if path == "/healthz" {
        "health"
    } else if path == "/readyz" {
        "ready"
    } else if path == "/metrics" {
        "metrics"
    } else if path.starts_with("/wms") {
        "wms"
    } else if path.starts_with("/wmts") {
        "wmts"
    } else {
        "other"
    }
}

// ---------- handlers ----------

async fn handle_wms(State(state): State<AppState>, headers: HeaderMap, raw_query: axum::extract::RawQuery) -> Response {
    let req_id = request_id(&state, &headers);
    let span = tracing::info_span!("wms", req_id = %req_id);

    async move {
        let raw = raw_query.0.unwrap_or_default();

        let parsed = match mars_wms::parse_request(&raw, &state.wms_cfg) {
            Ok(r) => r,
            Err(e) => return wms_error_response(e),
        };

        match parsed {
            WmsRequest::GetCapabilities => {
                let mut h = HeaderMap::new();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
                let body = state.capabilities.load_full();
                (StatusCode::OK, h, body.as_str().to_owned()).into_response()
            }
            WmsRequest::GetMap(plan) => {
                let mime = plan.format.mime();
                match state.runtime.render(&plan).await {
                    Ok(bytes) => {
                        let mut h = HeaderMap::new();
                        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                        (StatusCode::OK, h, bytes).into_response()
                    }
                    Err(e) => runtime_error_response(e, &plan),
                }
            }
        }
    }
    .instrument(span)
    .await
}

async fn handle_ready(State(state): State<AppState>) -> Response {
    if state.runtime.is_ready() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no manifest").into_response()
    }
}

async fn handle_metrics(State(state): State<AppState>) -> Response {
    match state.metrics.encode_text() {
        Ok(body) => {
            let mut h = HeaderMap::new();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            (StatusCode::OK, h, body).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "metrics encode failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics encode failed").into_response()
        }
    }
}

// ---------- helpers ----------

fn request_id(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-request-id").and_then(|h| h.to_str().ok()) {
        return v.to_owned();
    }
    let n = state.request_counter.fetch_add(1, Ordering::Relaxed);
    format!("req-{n}")
}

fn wms_error_response(e: WmsError) -> Response {
    let status = match e {
        WmsError::MissingParam(_) | WmsError::InvalidParam { .. } => StatusCode::BAD_REQUEST,
        WmsError::NotImplemented { .. } => StatusCode::NOT_IMPLEMENTED,
    };
    (status, e.to_string()).into_response()
}

fn runtime_error_response(e: RuntimeError, plan: &RenderPlan) -> Response {
    let status = match &e {
        RuntimeError::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeError::CrsNotCanonical { .. } => StatusCode::NOT_IMPLEMENTED,
        RuntimeError::ManifestEntryMissing { .. } | RuntimeError::SourceMissing { .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let cell = match &e {
        RuntimeError::ManifestEntryMissing { cell, .. } => Some(*cell),
        RuntimeError::SourceMissing { cell, .. } => Some(*cell),
        _ => None,
    };
    match &e {
        RuntimeError::NotReady => {
            tracing::warn!(error = %e, layers = ?plan.layers, bbox = ?plan.bbox, cell = ?cell, "render failed")
        }
        _ => {
            tracing::error!(error = %e, layers = ?plan.layers, bbox = ?plan.bbox, cell = ?cell, "render failed")
        }
    }
    let body = if status == StatusCode::INTERNAL_SERVER_ERROR {
        "internal server error".to_owned()
    } else {
        e.to_string()
    };
    (status, body).into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use mars_render_port::{
        Canvas, EncodeError, Encoder, ImageFormat as RenderImageFormat, Pixmap, RenderError, Renderer,
    };
    use mars_runtime::{Deps, RuntimeState};
    use mars_store::stub::{NotImplementedCache, NotImplementedStore};
    use mars_types::{CrsCode, ImageFormat, Manifest};
    use tower::ServiceExt;

    #[derive(Debug)]
    struct NoopRenderer;

    impl Renderer for NoopRenderer {
        fn render(&self, canvas: Canvas, _ops: &[mars_render_port::DrawOp]) -> Result<Pixmap, RenderError> {
            Ok(Pixmap {
                width: canvas.width,
                height: canvas.height,
                premultiplied_rgba: Vec::new(),
            })
        }
    }

    #[derive(Debug)]
    struct NoopEncoder;

    impl Encoder for NoopEncoder {
        fn encode(&self, _pixmap: &Pixmap, _format: RenderImageFormat) -> Result<Vec<u8>, EncodeError> {
            Ok(Vec::new())
        }
    }

    fn empty_runtime(metrics: &Metrics) -> Arc<Runtime> {
        Arc::new(Runtime::empty(Deps {
            store: Arc::new(NotImplementedStore),
            cache: Arc::new(NotImplementedCache),
            renderer: Arc::new(NoopRenderer),
            encoder: Arc::new(NoopEncoder),
            metrics: metrics.clone(),
        }))
    }

    fn empty_router() -> Router {
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let metrics = Metrics::new().unwrap();
        router(
            empty_runtime(&metrics),
            capabilities_handle("<caps/>".into()),
            cfg,
            metrics,
        )
    }

    fn ready_state() -> RuntimeState {
        RuntimeState {
            canonical_crs: CrsCode::new("EPSG:25832"),
            bands: Vec::new(),
            layer_order: Vec::new(),
            stylesheet: Default::default(),
            manifest: Manifest::new(1, "test", Vec::new(), Vec::new(), None, Vec::new()),
            layer_index: Default::default(),
            source_index: Default::default(),
        }
    }

    async fn body_str(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn healthz_ok() {
        let app = empty_router();
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_str(resp).await, "ok");
    }

    #[tokio::test]
    async fn readyz_503_without_manifest() {
        let app = empty_router();
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readyz_200_after_swap_state() {
        let metrics = Metrics::new().unwrap();
        let runtime = empty_runtime(&metrics);
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let app = router(runtime.clone(), capabilities_handle("<caps/>".into()), cfg, metrics);
        runtime.swap_state(Arc::new(ready_state()));

        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wms_capabilities_200() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned();
        assert_eq!(ct.unwrap(), "application/xml");
    }

    #[tokio::test]
    async fn wms_capabilities_reflects_swap() {
        let metrics = Metrics::new().unwrap();
        let caps = capabilities_handle("<caps>v1</caps>".into());
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let app = router(empty_runtime(&metrics), caps.clone(), cfg, metrics);
        caps.store(Arc::new("<caps>v2</caps>".to_owned()));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_str(resp).await.contains("v2"));
    }

    #[tokio::test]
    async fn wms_invalid_400() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?request=GetMap")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wms_get_map_503_without_manifest() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetMap&layers=a&styles=&crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn metrics_emits_prometheus_text() {
        let app = empty_router();
        // first request populates a counter line
        let _ = app
            .clone()
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/plain"));
        let body = body_str(resp).await;
        assert!(body.contains("mars_request_total"));
        assert!(body.contains("interface=\"health\""));
    }
}
