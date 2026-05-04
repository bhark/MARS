//! `axum`-backed HTTP edge.
//!
//! Routes (SPEC §3.3):
//!
//! ```text
//! /wms        WMS 1.3.0
//! /healthz    liveness
//! /readyz     readiness (gated on a usable manifest)
//! /metrics    Prometheus scrape (placeholder body in Phase 0)
//! ```
//!
//! WMTS lands in Phase 1 alongside the tile-cache.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mars_runtime::{Runtime, RuntimeError};
use mars_wms::{WmsConfig, WmsError, WmsRequest};
use tracing::Instrument;

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

/// Shared per-request state.
#[derive(Clone)]
struct AppState {
    runtime: Arc<Runtime>,
    capabilities: Arc<String>,
    wms_cfg: Arc<WmsConfig>,
    request_counter: Arc<AtomicU64>,
}

/// Build the router. Exposed for in-process testing via `tower::ServiceExt`.
pub fn router(runtime: Arc<Runtime>, capabilities: String, wms_cfg: WmsConfig) -> Router {
    let state = AppState {
        runtime,
        capabilities: Arc::new(capabilities),
        wms_cfg: Arc::new(wms_cfg),
        request_counter: Arc::new(AtomicU64::new(0)),
    };
    Router::new()
        .route("/wms", get(handle_wms))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/readyz", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .with_state(state)
}

/// Run the HTTP server until ctrl_c.
pub async fn serve(
    cfg: ServerConfig,
    runtime: Arc<Runtime>,
    capabilities_body: String,
    wms_cfg: WmsConfig,
) -> Result<(), HttpError> {
    let app = router(runtime, capabilities_body, wms_cfg);
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

// ---------- handlers ----------

async fn handle_wms(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_query: Option<axum::extract::RawQuery>,
) -> Response {
    let req_id = request_id(&state, &headers);
    let span = tracing::info_span!("wms", req_id = %req_id);

    async move {
        let raw = raw_query.and_then(|q| q.0).unwrap_or_default();

        let parsed = match mars_wms::parse_request(&raw, &state.wms_cfg) {
            Ok(r) => r,
            Err(e) => return wms_error_response(e),
        };

        match parsed {
            WmsRequest::GetCapabilities => {
                let mut h = HeaderMap::new();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
                (StatusCode::OK, h, state.capabilities.as_str().to_owned()).into_response()
            }
            WmsRequest::GetMap(plan) => {
                let mime = plan.format.mime();
                match state.runtime.render(&plan).await {
                    Ok(bytes) => {
                        let mut h = HeaderMap::new();
                        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                        (StatusCode::OK, h, bytes).into_response()
                    }
                    Err(e) => runtime_error_response(e),
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

async fn handle_metrics() -> Response {
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    (StatusCode::OK, h, "# phase-0 metrics not yet wired\n".to_owned()).into_response()
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

fn runtime_error_response(e: RuntimeError) -> Response {
    let status = match &e {
        RuntimeError::NotReady => StatusCode::SERVICE_UNAVAILABLE,
        RuntimeError::CrsNotCanonical { .. } => StatusCode::NOT_IMPLEMENTED,
        RuntimeError::ManifestEntryMissing { .. } | RuntimeError::SourceMissing { .. } => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    match &e {
        RuntimeError::NotReady => tracing::warn!(error = %e, "render failed"),
        _ => tracing::error!(error = %e, "render failed"),
    }
    (status, e.to_string()).into_response()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use mars_render_port::{Canvas, ImageFormat as RenderImageFormat, RenderError, Renderer};
    use mars_runtime::{Deps, RuntimeState};
    use mars_store::stub::{NotImplementedCache, NotImplementedStore};
    use mars_types::{CrsCode, ImageFormat, Manifest};
    use tower::ServiceExt;

    #[derive(Debug)]
    struct NoopRenderer;

    #[async_trait::async_trait]
    impl Renderer for NoopRenderer {
        async fn render(
            &self,
            _canvas: Canvas,
            _ops: &[mars_render_port::DrawOp],
            _format: RenderImageFormat,
        ) -> Result<Vec<u8>, RenderError> {
            Ok(Vec::new())
        }
    }

    fn empty_runtime() -> Arc<Runtime> {
        Arc::new(Runtime::empty(Deps {
            store: Arc::new(NotImplementedStore),
            cache: Arc::new(NotImplementedCache),
            renderer: Arc::new(NoopRenderer),
        }))
    }

    fn empty_router() -> Router {
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
        };
        router(empty_runtime(), "<caps/>".into(), cfg)
    }

    fn ready_state() -> RuntimeState {
        RuntimeState {
            canonical_crs: CrsCode::new("EPSG:25832"),
            bands: Vec::new(),
            layer_order: Vec::new(),
            stylesheet: Default::default(),
            manifest: Manifest {
                version: 1,
                service: "test".into(),
                source_artifacts: Vec::new(),
                layer_artifacts: Vec::new(),
                style_artifact: None,
            },
            layer_index: Default::default(),
            source_index: Default::default(),
        }
    }

    async fn body_str(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
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
        let runtime = empty_runtime();
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
        };
        let app = router(runtime.clone(), "<caps/>".into(), cfg);
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
    async fn metrics_placeholder() {
        let app = empty_router();
        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_str(resp).await.contains("phase-0 metrics"));
    }
}
