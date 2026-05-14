//! `axum`-backed HTTP edge.
//!
//! Routes:
//!
//! ```text
//! /wms                                                     WMS 1.3.0
//! /wmts                                                    WMTS 1.0.0 KVP
//! /wmts/{Layer}/{Style}/{TMS}/{z}/{y}/{x}.{ext}            WMTS 1.0.0 REST
//! /healthz                                                 liveness
//! /readyz                                                  readiness (gated on a usable manifest)
//! /metrics                                                 Prometheus scrape
//! ```

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use mars_observability::Metrics;
use mars_runtime::Runtime;
use mars_wms::WmsConfig;
use mars_wmts::WmtsConfig;
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

mod errors;
mod handlers;
mod middleware;

pub use errors::*;
pub use handlers::*;
pub use middleware::*;

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

/// Atomically swappable capabilities document. Cheap clone, lock-free reads.
pub type CapabilitiesHandle = Arc<ArcSwap<CapabilitiesDoc>>;

/// Helper to build a fresh [`CapabilitiesHandle`] seeded with `body`.
#[must_use]
pub fn capabilities_handle(body: String) -> CapabilitiesHandle {
    Arc::new(ArcSwap::from(Arc::new(CapabilitiesDoc::new(body))))
}

/// Bundle of per-interface capabilities handles. Travel together through
/// `router` / `serve` so the signature stays narrow as more interfaces land.
#[derive(Clone)]
pub struct CapabilitiesBundle {
    pub wms: CapabilitiesHandle,
    pub wmts: CapabilitiesHandle,
}

/// Shared per-request state.
#[derive(Clone)]
pub struct AppState {
    runtime: Arc<Runtime>,
    wms_capabilities: CapabilitiesHandle,
    wmts_capabilities: CapabilitiesHandle,
    wms_cfg: Arc<WmsConfig>,
    wmts_cfg: Arc<WmtsConfig>,
    metrics: Metrics,
    request_counter: Arc<AtomicU64>,
}

/// Build the router. Exposed for in-process testing via `tower::ServiceExt`.
pub fn router(
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesBundle,
    wms_cfg: WmsConfig,
    wmts_cfg: WmtsConfig,
    metrics: Metrics,
) -> Router {
    let state = AppState {
        runtime,
        wms_capabilities: capabilities.wms,
        wmts_capabilities: capabilities.wmts,
        wms_cfg: Arc::new(wms_cfg),
        wmts_cfg: Arc::new(wmts_cfg),
        metrics,
        request_counter: Arc::new(AtomicU64::new(0)),
    };
    Router::new()
        .route("/wms", get(handle_wms))
        .route("/wmts", get(handle_wmts))
        .route("/wmts/{layer}/{style}/{tms}/{z}/{y}/{x_ext}", get(handle_wmts_rest))
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/readyz", get(handle_ready))
        .route("/metrics", get(handle_metrics))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, observe_request))
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
    capabilities: CapabilitiesBundle,
    wms_cfg: WmsConfig,
    wmts_cfg: WmtsConfig,
    metrics: Metrics,
    shutdown: CancellationToken,
) -> Result<(), HttpError> {
    let app = router(runtime, capabilities, wms_cfg, wmts_cfg, metrics);
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use axum::response::Response;
    use mars_render_port::{
        Canvas, EncodeError, Encoder, ImageFormat as RenderImageFormat, Pixmap, RenderError, Renderer,
    };
    use mars_runtime::{Deps, RuntimeState};
    use mars_store::stub::{NotImplementedCache, NotImplementedStore};
    use mars_types::{CrsCode, ImageFormat};
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

        fn measure_text(
            &self,
            text: &str,
            style: &mars_style::LabelStyle,
        ) -> Result<mars_render_port::TextMetrics, RenderError> {
            // endpoint tests don't exercise the collision pass; coarse stub.
            let chars = text.chars().count().max(1) as f32;
            let fs = style.font_size.max(1.0);
            Ok(mars_render_port::TextMetrics {
                advance_x: chars * 0.55 * fs,
                ascent: fs * 0.8,
                descent: fs * 0.2,
            })
        }
    }

    #[derive(Debug)]
    struct NoopEncoder;

    impl Encoder for NoopEncoder {
        fn encode(&self, pixmap: &Pixmap, _format: RenderImageFormat) -> Result<Vec<u8>, EncodeError> {
            // emit a non-empty byte stream so callers can distinguish a blank
            // (encoded transparent canvas) from an empty payload. dimensions
            // are echoed so tests can assert the blank carried the request size.
            let mut out = b"BLANK".to_vec();
            out.extend_from_slice(&pixmap.width.to_le_bytes());
            out.extend_from_slice(&pixmap.height.to_le_bytes());
            Ok(out)
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
            images: Arc::new(mars_runtime::images::MutableImageRegistry::new()),
            raster_sources: std::collections::HashMap::new(),
        }))
    }

    fn empty_router() -> Router {
        let metrics = Metrics::new().unwrap();
        router(
            empty_runtime(&metrics),
            CapabilitiesBundle {
                wms: capabilities_handle("<wmscaps/>".into()),
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            test_wms_cfg(),
            test_wmts_cfg(),
            metrics,
        )
    }

    fn test_wms_cfg() -> WmsConfig {
        WmsConfig {
            allowlist_crs: vec![CrsCode::new("EPSG:25832")],
            formats: vec![ImageFormat::Png],
            max_image_dimension: 8192,
            max_pixels: 16_000_000,
            max_layers: 100,
            max_bbox_coord: 1e9,
            scale_pixel_size_m: 0.0254 / 96.0,
        }
    }

    fn test_wmts_cfg() -> WmtsConfig {
        use mars_config::{TileMatrixLevel, TileMatrixSet};
        let mut sets = std::collections::BTreeMap::new();
        sets.insert(
            "dk_25832".to_owned(),
            TileMatrixSet {
                crs: CrsCode::new("EPSG:25832"),
                top_left: [0.0, 1024.0],
                tile_size: [16, 16],
                // sd chosen so pixel_size_units = 1.0; 16-tile spans 16 units.
                levels: vec![TileMatrixLevel {
                    id: 0,
                    scale_denominator: 1.0 / 0.000_28,
                    matrix_width: 1,
                    matrix_height: 1,
                }],
            },
        );
        WmtsConfig {
            tile_matrix_sets: sets,
            formats: vec![ImageFormat::Png],
            max_bbox_coord: 1e9,
        }
    }

    fn ready_state() -> RuntimeState {
        RuntimeState::empty(1, "test")
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
    async fn wmts_no_request_param_is_400() {
        // a bare /wmts hit with no `request=` is a missing-parameter error,
        // returned as an OWS ExceptionReport (not a WMS ServiceExceptionReport).
        let app = empty_router();
        let resp = app
            .oneshot(Request::builder().uri("/wmts").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains("ExceptionReport"));
        assert!(!body.contains("ServiceExceptionReport"));
        assert!(body.contains(r#"exceptionCode="MissingParameterValue""#));
    }

    #[tokio::test]
    async fn wmts_get_capabilities_200() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts?service=WMTS&version=1.0.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
        assert_eq!(ct, "application/xml");
        assert!(resp.headers().get(header::ETAG).is_some(), "etag header expected");
        let body = body_str(resp).await;
        // empty_router seeds with literal "<wmtscaps/>"; the WMS handle holds
        // a different body so this confirms the right handle was selected.
        assert!(body.contains("<wmtscaps/>"));
        assert!(!body.contains("<wmscaps/>"));
    }

    #[tokio::test]
    async fn wmts_capabilities_304_on_matching_etag() {
        let app = empty_router();
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wmts?service=WMTS&version=1.0.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = first.headers().get(header::ETAG).cloned().unwrap();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts?service=WMTS&version=1.0.0&request=GetCapabilities")
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
    async fn wmts_get_tile_503_without_manifest() {
        // a syntactically valid GetTile parses cleanly; the runtime then
        // responds 503 because no manifest is loaded.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wmts?service=WMTS&version=1.0.0&request=GetTile&layer=a&style=&\
                         format=image/png&tilematrixset=dk_25832&tilematrix=0&tilecol=0&tilerow=0",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_str(resp).await;
        assert!(body.contains("ExceptionReport"));
        assert!(!body.contains("ServiceExceptionReport"));
    }

    #[tokio::test]
    async fn wmts_rest_get_tile_503_without_manifest() {
        // a syntactically valid REST GetTile parses cleanly; the runtime then
        // responds 503 because no manifest is loaded.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts/a/default/dk_25832/0/0/0.png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_str(resp).await;
        assert!(body.contains("ExceptionReport"));
    }

    #[tokio::test]
    async fn wmts_rest_invalid_ext_400() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts/a/default/dk_25832/0/0/0.tiff")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains(r#"exceptionCode="InvalidParameterValue""#));
        assert!(body.contains(r#"locator="format""#));
    }

    #[tokio::test]
    async fn wmts_rest_missing_ext_400() {
        // path captures a final segment without a `.`; the handler rejects
        // before reaching the parser.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts/a/default/dk_25832/0/0/0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wmts_rest_unknown_tms_400_with_locator() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts/a/default/nope/0/0/0.png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains(r#"locator="tilematrixset""#));
    }

    #[tokio::test]
    async fn wmts_invalid_tms_is_400_with_locator() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wmts?request=GetTile&layer=a&format=image/png&tilematrixset=nope&\
                         tilematrix=0&tilecol=0&tilerow=0",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains(r#"exceptionCode="InvalidParameterValue""#));
        assert!(body.contains(r#"locator="tilematrixset""#));
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
        let app = router(
            runtime.clone(),
            CapabilitiesBundle {
                wms: capabilities_handle("<wmscaps/>".into()),
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            test_wms_cfg(),
            test_wmts_cfg(),
            metrics,
        );
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
        let app = router(
            empty_runtime(&metrics),
            CapabilitiesBundle {
                wms: caps.clone(),
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            test_wms_cfg(),
            test_wmts_cfg(),
            metrics,
        );
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
    async fn wms_get_legend_graphic_503_without_manifest() {
        // legend rendering needs config from the active state; without a
        // manifest the runtime returns NotReady and the handler maps it to a
        // 503 XML response.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetLegendGraphic&layer=a&format=image/png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
    }

    #[tokio::test]
    async fn wms_get_legend_graphic_missing_layer_400() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetLegendGraphic&format=image/png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn wms_get_feature_info_503_without_manifest() {
        // a syntactically valid GFI request parses cleanly and dispatches
        // through the gfi path; with no manifest the runtime returns NotReady
        // which the handler translates to a 503 XML exception.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wms?service=WMS&version=1.3.0&request=GetFeatureInfo&layers=a&styles=&\
                         crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png&\
                         query_layers=a&info_format=text/plain&i=8&j=8",
                    )
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
    }

    #[tokio::test]
    async fn wms_get_feature_info_invalid_query_layers_400() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wms?service=WMS&version=1.3.0&request=GetFeatureInfo&layers=a&styles=&\
                         crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png&\
                         query_layers=z&info_format=text/plain&i=8&j=8",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_str(resp).await;
        assert!(body.contains(r#"code="InvalidParameterValue""#));
    }

    #[tokio::test]
    async fn wms_exceptions_blank_returns_200_image_on_runtime_error() {
        // exceptions=BLANK suppresses the XML error report; the runtime's
        // NotReady error must be converted into a 200 OK image of the
        // requested dimensions instead.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wms?service=WMS&version=1.3.0&request=GetMap&layers=a&styles=&\
                         crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png&exceptions=BLANK",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
        assert_eq!(ct, "image/png");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        // NoopEncoder echoes "BLANK" + width + height; assert the blank ran
        // through it at the requested dimensions.
        assert!(body.starts_with(b"BLANK"), "expected encoder fallthrough");
        let mut w = [0u8; 4];
        w.copy_from_slice(&body[5..9]);
        assert_eq!(u32::from_le_bytes(w), 16);
    }

    #[tokio::test]
    async fn wms_exceptions_xml_returns_service_exception() {
        // sanity inverse of the blank test: default exceptions=XML keeps the
        // existing behaviour.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wms?service=WMS&version=1.3.0&request=GetMap&layers=a&styles=&\
                         crs=EPSG:25832&bbox=0,0,10,10&width=16&height=16&format=image/png&exceptions=XML",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_str(resp).await;
        assert!(body.contains("ServiceExceptionReport"));
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
        assert!(body.contains(r#"status="4xx""#));
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
