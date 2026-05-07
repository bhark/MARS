//! `axum`-backed HTTP edge.
//!
//! Routes (SPEC §3.3):
//!
//! ```text
//! /wms        WMS 1.3.0
//! /wmts       reserved (501 until Phase 1 tile-cache lands)
//! /healthz    liveness
//! /readyz     readiness (gated on a usable manifest)
//! /metrics    Prometheus scrape
//! ```

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
use mars_grid::GridError;
use mars_observability::Metrics;
use mars_runtime::{RenderPlan, Runtime, RuntimeError};
use mars_wms::{WmsConfig, WmsError, WmsRequest};
use tokio_util::sync::CancellationToken;
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
}

/// Capabilities document with a precomputed strong ETag. `body` is held as
/// `Bytes` so the per-request response can clone a refcount instead of the
/// underlying buffer on every GetCapabilities hit.
#[derive(Debug)]
pub struct CapabilitiesDoc {
    pub body: bytes::Bytes,
    pub etag: String,
}

impl CapabilitiesDoc {
    #[must_use]
    pub fn new(body: String) -> Self {
        let etag = etag_for(body.as_bytes());
        Self {
            body: bytes::Bytes::from(body),
            etag,
        }
    }
}

fn etag_for(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    // strong validator, hex-truncated to 16 chars (64 bits) - collision-safe for caps.
    format!("\"{}\"", &hash.to_hex().as_str()[..16])
}

/// Atomically swappable capabilities document. Cheap clone, lock-free reads.
pub type CapabilitiesHandle = Arc<ArcSwap<CapabilitiesDoc>>;

/// Helper to build a fresh [`CapabilitiesHandle`] seeded with `body`.
#[must_use]
pub fn capabilities_handle(body: String) -> CapabilitiesHandle {
    Arc::new(ArcSwap::from(Arc::new(CapabilitiesDoc::new(body))))
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
        .route("/wmts", get(handle_wmts_not_implemented))
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

/// Run the HTTP server until `shutdown` is cancelled. The caller is
/// responsible for installing a signal handler that triggers the token.
pub async fn serve(
    cfg: ServerConfig,
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesHandle,
    wms_cfg: WmsConfig,
    metrics: Metrics,
    shutdown: CancellationToken,
) -> Result<(), HttpError> {
    let app = router(runtime, capabilities, wms_cfg, metrics);
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| HttpError::Listen(format!("bind {}: {e}", cfg.listen)))?;
    tracing::info!(addr = %cfg.listen, "http: listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            tracing::info!("http: shutdown requested");
        })
        .await
        .map_err(|e| HttpError::Listen(e.to_string()))
}

// ---------- middleware ----------

async fn observe_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let interface = interface_label(req.uri().path());
    let start = Instant::now();
    let resp = next.run(req).await;
    state
        .metrics
        .observe_request(interface, resp.status().as_u16(), start.elapsed());
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
                let doc = state.capabilities.load_full();
                let etag_value = match HeaderValue::from_str(&doc.etag) {
                    Ok(v) => v,
                    Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "bad etag").into_response(),
                };
                if let Some(req_etag) = headers.get(header::IF_NONE_MATCH)
                    && req_etag == etag_value
                {
                    let mut h = HeaderMap::new();
                    h.insert(header::ETAG, etag_value);
                    return (StatusCode::NOT_MODIFIED, h).into_response();
                }
                let mut h = HeaderMap::new();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
                h.insert(header::ETAG, etag_value);
                (StatusCode::OK, h, doc.body.clone()).into_response()
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

/// /wmts is reserved for the WMTS interface that lands with the tile cache.
/// Until then, return a clean 501 so probing clients get a typed answer
/// rather than a 404 that suggests the route does not exist.
async fn handle_wmts_not_implemented() -> Response {
    let exc = WmsException {
        status: StatusCode::NOT_IMPLEMENTED,
        code: Some("OperationNotSupported"),
        message: "WMTS is not yet implemented".into(),
    };
    wms_exception_response(exc)
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

/// Hard cap on incoming `x-request-id`: long enough for UUIDs, short enough
/// to keep structured-log cardinality and per-line size bounded.
const REQUEST_ID_MAX_LEN: usize = 128;

fn request_id(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-request-id").and_then(|h| h.to_str().ok()) {
        // accept only printable ascii (plus space-equivalents) within the cap.
        // anything else falls back to a counter so a malicious client cannot
        // inject newlines into structured logs or blow the per-line budget.
        if (1..=REQUEST_ID_MAX_LEN).contains(&v.len())
            && v.bytes().all(|b| matches!(b, 0x21..=0x7e))
        {
            return v.to_owned();
        }
    }
    let n = state.request_counter.fetch_add(1, Ordering::Relaxed);
    format!("req-{n}")
}

struct WmsException {
    status: StatusCode,
    code: Option<&'static str>,
    message: String,
}

fn wms_exception_response(exc: WmsException) -> Response {
    let xml = mars_wms::service_exception_report(exc.code, &exc.message);
    let mut resp = (exc.status, xml).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/xml; charset=utf-8"),
    );
    resp
}

fn wms_error_response(e: WmsError) -> Response {
    let exc = match e {
        WmsError::MissingParam(name) => WmsException {
            status: StatusCode::BAD_REQUEST,
            code: Some("MissingParameterValue"),
            message: format!("Missing required parameter: {name}"),
        },
        WmsError::InvalidParam { name, reason } => WmsException {
            status: StatusCode::BAD_REQUEST,
            code: Some("InvalidParameterValue"),
            message: format!("Invalid parameter '{name}': {reason}"),
        },
        WmsError::NotImplemented { what } => WmsException {
            status: StatusCode::NOT_IMPLEMENTED,
            code: Some("OperationNotSupported"),
            message: format!("Operation not supported: {what}"),
        },
    };
    wms_exception_response(exc)
}

fn runtime_error_response(e: RuntimeError, plan: &RenderPlan) -> Response {
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
    wms_exception_response(map_runtime_error(&e))
}

fn map_runtime_error(e: &RuntimeError) -> WmsException {
    match e {
        RuntimeError::NotReady => WmsException {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: None,
            message: "Service temporarily unavailable".into(),
        },
        RuntimeError::Proj(proj_err) => match proj_err {
            mars_proj::ProjError::UnknownCrs(name) => WmsException {
                status: StatusCode::BAD_REQUEST,
                code: Some("InvalidCRS"),
                message: format!("CRS '{name}' is not supported"),
            },
            _ => WmsException {
                status: StatusCode::BAD_REQUEST,
                code: Some("InvalidCRS"),
                message: "Coordinate transformation failed".into(),
            },
        },
        RuntimeError::NotImplemented { what } => WmsException {
            status: StatusCode::NOT_IMPLEMENTED,
            code: Some("OperationNotSupported"),
            message: format!("Operation not supported: {what}"),
        },
        RuntimeError::LayerNotDefined { layer } => WmsException {
            status: StatusCode::BAD_REQUEST,
            code: Some("LayerNotDefined"),
            message: format!("Layer '{layer}' is not defined"),
        },
        RuntimeError::Grid(grid_err) => match grid_err {
            GridError::TooManyCells { requested, limit } => WmsException {
                status: StatusCode::BAD_REQUEST,
                code: Some("InvalidParameterValue"),
                message: format!("Request covers too many cells ({requested} > {limit})"),
            },
            GridError::NoBandForScale(denom) => WmsException {
                status: StatusCode::BAD_REQUEST,
                code: Some("InvalidParameterValue"),
                message: format!("Scale {denom} is not supported"),
            },
            _ => WmsException {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: None,
                message: "Internal server error".into(),
            },
        },
        RuntimeError::PixelBudgetExceeded { requested, budget } => WmsException {
            status: StatusCode::BAD_REQUEST,
            code: Some("InvalidParameterValue"),
            message: format!("Request requires {requested} pixels but server budget is {budget}"),
        },
        // SPEC §8.5: a cell missing from both lists is a broken manifest;
        // surface as 404 so clients can distinguish "no data" from "we broke".
        RuntimeError::ManifestEntryMissing { .. } | RuntimeError::SourceMissing { .. } => WmsException {
            status: StatusCode::NOT_FOUND,
            code: Some("LayerNotQueryable"),
            message: "No data available for the requested area".into(),
        },
        RuntimeError::BadKey { .. }
        | RuntimeError::Config(_)
        | RuntimeError::Store(_)
        | RuntimeError::Render(_)
        | RuntimeError::Encode(_)
        | RuntimeError::Artifact(_) => WmsException {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: None,
            message: "Internal server error".into(),
        },
    }
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
            fonts: Arc::new(mars_runtime::Fonts::with_default()),
        }))
    }

    fn empty_router() -> Router {
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
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
    async fn wmts_returns_501_with_service_exception() {
        let app = empty_router();
        let resp = app
            .oneshot(Request::builder().uri("/wmts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
        assert!(body.contains("OperationNotSupported"));
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
            max_pixels: 16_000_000,
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
        assert!(resp.headers().get(header::ETAG).is_some(), "etag header expected");
    }

    #[tokio::test]
    async fn wms_capabilities_304_on_matching_etag() {
        let app = empty_router();
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = first.headers().get(header::ETAG).cloned().unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetCapabilities")
                    .header(header::IF_NONE_MATCH, etag.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers().get(header::ETAG).cloned().unwrap(), etag);
        assert!(body_str(resp).await.is_empty());
    }

    #[tokio::test]
    async fn wms_capabilities_reflects_swap() {
        let metrics = Metrics::new().unwrap();
        let caps = capabilities_handle("<caps>v1</caps>".into());
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let app = router(empty_runtime(&metrics), caps.clone(), cfg, metrics);
        caps.store(Arc::new(CapabilitiesDoc::new("<caps>v2</caps>".to_owned())));
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

    fn ready_state_with_band() -> RuntimeState {
        RuntimeState {
            canonical_crs: CrsCode::new("EPSG:25832"),
            bands: vec![mars_grid::BandConfig {
                name: mars_types::ScaleBand::new("hi"),
                max_denom: u32::MAX,
                origin: (0.0, 0.0),
                cell_size: 1024.0,
            }],
            layer_order: vec![mars_types::LayerId::new("a")],
            stylesheet: Default::default(),
            manifest: Manifest::new(1, "test", Vec::new(), Vec::new(), None, Vec::new()),
            layer_index: Default::default(),
            source_index: Default::default(),
        }
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
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/xml"));
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
        assert!(body.contains(r#"code="MissingParameterValue""#));
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
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/xml"));
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
        assert!(!body.contains("code="));
    }

    #[tokio::test]
    async fn wms_unknown_layer_returns_layer_not_defined() {
        let metrics = Metrics::new().unwrap();
        let runtime = empty_runtime(&metrics);
        let mut state = ready_state_with_band();
        state.layer_order = vec![mars_types::LayerId::new("b")];
        runtime.swap_state(Arc::new(state));
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let app = router(runtime, capabilities_handle("<caps/>".into()), cfg, metrics);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetMap&layers=a&styles=&crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains(r#"code="LayerNotDefined""#));
        assert!(body.contains("Layer &apos;a&apos; is not defined"));
    }

    #[tokio::test]
    async fn wms_manifest_entry_missing_sanitized() {
        let metrics = Metrics::new().unwrap();
        let runtime = empty_runtime(&metrics);
        runtime.swap_state(Arc::new(ready_state_with_band()));
        let cfg = WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
        };
        let app = router(runtime, capabilities_handle("<caps/>".into()), cfg, metrics);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetMap&layers=a&styles=&crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
        assert!(body.contains(r#"code="LayerNotQueryable""#));
        assert!(body.contains("No data available"));
        // never leak internal cell coordinates / file paths to the client.
        assert!(!body.contains("manifest entry missing"));
        assert!(!body.contains("cell ("));
    }

    #[tokio::test]
    async fn wms_bad_request_records_semantic_400_in_metrics() {
        let app = empty_router();
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wms?request=GetMap")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_str(resp).await;
        assert!(body.contains(r#"interface="wms""#));
        assert!(body.contains(r#"status="400""#));
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
