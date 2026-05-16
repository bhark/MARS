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
use mars_config::CorsConfig;
use mars_observability::Metrics;
use mars_runtime::Runtime;
use mars_wms::{WmsConfig, WmsVersion};
use mars_wmts::WmtsConfig;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
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

/// Per-interface configuration bundle threaded into [`router`] and
/// [`serve`]. Grouping the WMS / WMTS / CORS knobs keeps both entry-point
/// signatures bounded as more interfaces land.
pub struct InterfacesConfig {
    pub wms: WmsConfig,
    pub wmts: WmtsConfig,
    pub cors: Option<CorsConfig>,
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

/// Per-version WMS capabilities handles. The HTTP edge serves the document
/// matching the negotiated [`WmsVersion`]; both are atomically swappable on
/// manifest changes so a 1.1.1 client and a 1.3.0 client never observe a
/// stale half-update.
#[derive(Clone)]
pub struct WmsCapabilitiesHandles {
    pub v111: CapabilitiesHandle,
    pub v130: CapabilitiesHandle,
}

impl WmsCapabilitiesHandles {
    /// Look up the cached capabilities document for the negotiated version.
    #[must_use]
    pub fn for_version(&self, version: WmsVersion) -> &CapabilitiesHandle {
        match version {
            WmsVersion::V111 => &self.v111,
            WmsVersion::V130 => &self.v130,
        }
    }
}

/// Bundle of per-interface capabilities handles. Travel together through
/// `router` / `serve` so the signature stays narrow as more interfaces land.
#[derive(Clone)]
pub struct CapabilitiesBundle {
    pub wms: WmsCapabilitiesHandles,
    pub wmts: CapabilitiesHandle,
}

/// Shared per-request state.
#[derive(Clone)]
pub struct AppState {
    runtime: Arc<Runtime>,
    wms_capabilities: WmsCapabilitiesHandles,
    wmts_capabilities: CapabilitiesHandle,
    wms_cfg: Arc<WmsConfig>,
    wmts_cfg: Arc<WmtsConfig>,
    metrics: Metrics,
    request_counter: Arc<AtomicU64>,
}

/// Build the router. Exposed for in-process testing via `tower::ServiceExt`.
/// When `interfaces.cors` is `Some`, a [`CorsLayer`] is mounted on every
/// route; when `None` no CORS headers are emitted (matches the prior
/// default).
pub fn router(
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesBundle,
    interfaces: InterfacesConfig,
    metrics: Metrics,
) -> Router {
    let InterfacesConfig {
        wms: wms_cfg,
        wmts: wmts_cfg,
        cors,
    } = interfaces;
    let state = AppState {
        runtime,
        wms_capabilities: capabilities.wms,
        wmts_capabilities: capabilities.wmts,
        wms_cfg: Arc::new(wms_cfg),
        wmts_cfg: Arc::new(wmts_cfg),
        metrics,
        request_counter: Arc::new(AtomicU64::new(0)),
    };
    let mut router = Router::new()
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
        ));
    if let Some(layer) = cors.and_then(build_cors_layer) {
        router = router.layer(layer);
    }
    router
}

/// Translate a [`CorsConfig`] into a [`CorsLayer`]. Returns `None` when the
/// allowlist is empty (no policy worth applying); the bin treats this as a
/// validation error elsewhere.
fn build_cors_layer(cfg: CorsConfig) -> Option<CorsLayer> {
    if cfg.allow_origins.is_empty() {
        return None;
    }
    let mut layer = CorsLayer::new();
    if cfg.allow_origins.iter().any(|o| o == "*") {
        layer = layer.allow_origin(AllowOrigin::any());
    } else {
        let parsed: Vec<axum::http::HeaderValue> = cfg
            .allow_origins
            .iter()
            .filter_map(|o| axum::http::HeaderValue::from_str(o).ok())
            .collect();
        if parsed.is_empty() {
            return None;
        }
        layer = layer.allow_origin(AllowOrigin::list(parsed));
    }
    let methods: Vec<axum::http::Method> = cfg
        .allow_methods
        .iter()
        .filter_map(|m| axum::http::Method::from_bytes(m.as_bytes()).ok())
        .collect();
    if !methods.is_empty() {
        layer = layer.allow_methods(methods);
    }
    if let Some(secs) = cfg.max_age_seconds {
        layer = layer.max_age(Duration::from_secs(secs));
    }
    Some(layer)
}

/// Run the HTTP server until `shutdown` is cancelled. The caller is
/// responsible for installing a signal handler that triggers the token.
pub async fn serve(
    cfg: ServerConfig,
    runtime: Arc<Runtime>,
    capabilities: CapabilitiesBundle,
    interfaces: InterfacesConfig,
    metrics: Metrics,
    shutdown: CancellationToken,
) -> Result<(), HttpError> {
    let app = router(runtime, capabilities, interfaces, metrics);
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
        router_with_cors(None)
    }

    fn router_with_cors(cors: Option<CorsConfig>) -> Router {
        let metrics = Metrics::new().unwrap();
        router(
            empty_runtime(&metrics),
            CapabilitiesBundle {
                wms: WmsCapabilitiesHandles {
                    v111: capabilities_handle("<wmscaps111/>".into()),
                    v130: capabilities_handle("<wmscaps/>".into()),
                },
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            InterfacesConfig {
                wms: test_wms_cfg(),
                wmts: test_wmts_cfg(),
                cors,
            },
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
            layer_policies: std::collections::BTreeMap::new(),
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

    /// Build a `/wms?...` URI for a parametric test body. `extra` carries
    /// neutral logical keys (`"crs"`, `"i"`, `"j"`); the helper rewrites to
    /// the version-appropriate wire keys (`"srs"`, `"x"`, `"y"` under 1.1.1)
    /// so call sites read identically across versions.
    fn wms_uri(version: &str, request: &str, extra: &[(&str, &str)]) -> String {
        let mut out = format!("/wms?service=WMS&version={version}&request={request}");
        for (k, v) in extra {
            let key = match (*k, version) {
                ("crs", "1.1.1") => "srs",
                ("i", "1.1.1") => "x",
                ("j", "1.1.1") => "y",
                _ => k,
            };
            out.push('&');
            out.push_str(key);
            out.push('=');
            out.push_str(v);
        }
        out
    }

    const WMS_VERSIONS: &[&str] = &["1.3.0", "1.1.1"];

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
                wms: WmsCapabilitiesHandles {
                    v111: capabilities_handle("<wmscaps111/>".into()),
                    v130: capabilities_handle("<wmscaps/>".into()),
                },
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            InterfacesConfig {
                wms: test_wms_cfg(),
                wmts: test_wmts_cfg(),
                cors: None,
            },
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
    async fn wms_capabilities_304_on_matching_etag_per_version() {
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = format!("/wms?service=WMS&version={version}&request=GetCapabilities");
            let first = app
                .clone()
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let etag = first.headers().get(header::ETAG).cloned().unwrap();
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri(uri.as_str())
                        .header(header::IF_NONE_MATCH, etag.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "{version}");
            assert_eq!(resp.headers().get(header::ETAG).cloned().unwrap(), etag, "{version}");
            assert!(body_str(resp).await.is_empty(), "{version}");
        }
    }

    #[tokio::test]
    async fn wms_capabilities_reflects_swap() {
        let metrics = Metrics::new().unwrap();
        let caps = capabilities_handle("<caps>v1</caps>".into());
        let app = router(
            empty_runtime(&metrics),
            CapabilitiesBundle {
                wms: WmsCapabilitiesHandles {
                    v111: capabilities_handle("<caps111>v1</caps111>".into()),
                    v130: caps.clone(),
                },
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            InterfacesConfig {
                wms: test_wms_cfg(),
                wmts: test_wmts_cfg(),
                cors: None,
            },
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
        // default version tag when the client did not pin one
        assert!(body.contains(r#"version="1.3.0""#));
    }

    #[tokio::test]
    async fn cors_absent_means_no_header() {
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "https://example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.headers().get("access-control-allow-origin").is_none());
    }

    #[tokio::test]
    async fn cors_wildcard_reflects_any_origin() {
        let app = router_with_cors(Some(CorsConfig {
            allow_origins: vec!["*".to_owned()],
            allow_methods: vec!["GET".to_owned(), "HEAD".to_owned()],
            max_age_seconds: Some(600),
        }));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "https://example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let allow = resp.headers().get("access-control-allow-origin").cloned();
        assert_eq!(allow.as_ref().map(|v| v.to_str().unwrap()), Some("*"));
    }

    #[tokio::test]
    async fn cors_explicit_origin_reflects_match() {
        let app = router_with_cors(Some(CorsConfig {
            allow_origins: vec!["https://maps.example.org".to_owned()],
            allow_methods: vec!["GET".to_owned()],
            max_age_seconds: None,
        }));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "https://maps.example.org")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let allow = resp.headers().get("access-control-allow-origin").cloned();
        assert_eq!(
            allow.as_ref().map(|v| v.to_str().unwrap()),
            Some("https://maps.example.org")
        );
    }

    #[tokio::test]
    async fn wms_111_capabilities_served_separately() {
        // negotiate version=1.1.1 and confirm we get the v111-tagged stub
        // rather than the 1.3.0 document.
        let app = empty_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.1.1&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_str(resp).await;
        assert!(body.contains("<wmscaps111/>"), "got: {body}");
    }

    #[tokio::test]
    async fn wms_error_envelope_tags_negotiated_version() {
        // both pinned versions must round-trip into the ServiceExceptionReport
        // envelope. supersedes the 1.3.0-only wms_error_tags_requested_version.
        for version in WMS_VERSIONS {
            let app = empty_router();
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri(format!("/wms?service=WMS&version={version}&request=GetMap").as_str())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{version}");
            let body = body_str(resp).await;
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
            assert!(body.contains(r#"code="MissingParameterValue""#), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_get_map_503_without_manifest_per_version() {
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetMap",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "16"),
                    ("height", "16"),
                    ("format", "image/png"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{version}");
            let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
            assert!(ct.to_str().unwrap().starts_with("text/xml"), "{version}");
            let body = body_str(resp).await;
            assert!(body.contains("ServiceExceptionReport"), "{version}: {body}");
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
            // NotReady carries no code attribute by design.
            assert!(!body.contains("code="), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_get_legend_graphic_503_without_manifest_per_version() {
        // legend rendering needs config from the active state; without a
        // manifest the runtime returns NotReady and the handler maps it to a
        // 503 XML response. the envelope must carry the negotiated version.
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = format!("/wms?service=WMS&version={version}&request=GetLegendGraphic&layer=a&format=image/png");
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{version}");
            let body = body_str(resp).await;
            assert!(body.contains("ServiceExceptionReport"), "{version}: {body}");
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_get_legend_graphic_missing_layer_400_per_version() {
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = format!("/wms?service=WMS&version={version}&request=GetLegendGraphic&format=image/png");
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{version}");
            let body = body_str(resp).await;
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_get_feature_info_503_without_manifest_per_version() {
        // syntactically valid GFI for both versions parses cleanly and
        // dispatches through the gfi path; with no manifest the runtime
        // returns NotReady which the handler translates to 503 XML.
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetFeatureInfo",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "16"),
                    ("height", "16"),
                    ("format", "image/png"),
                    ("query_layers", "a"),
                    ("info_format", "text/plain"),
                    ("i", "8"),
                    ("j", "8"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{version}");
            let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
            assert!(ct.to_str().unwrap().starts_with("text/xml"), "{version}");
            let body = body_str(resp).await;
            assert!(body.contains("ServiceExceptionReport"), "{version}: {body}");
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_get_feature_info_invalid_query_layers_400_per_version() {
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetFeatureInfo",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "16"),
                    ("height", "16"),
                    ("format", "image/png"),
                    ("query_layers", "z"),
                    ("info_format", "text/plain"),
                    ("i", "8"),
                    ("j", "8"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{version}");
            let body = body_str(resp).await;
            assert!(body.contains(r#"code="InvalidParameterValue""#), "{version}: {body}");
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
        }
    }

    #[tokio::test]
    async fn wms_exceptions_inimage_returns_200_image_per_version() {
        // exceptions=INIMAGE renders the error message onto a transparent
        // image of the requested dimensions instead of XML; behaviour must
        // be identical for 1.1.1 and 1.3.0.
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetMap",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "64"),
                    ("height", "64"),
                    ("format", "image/png"),
                    ("exceptions", "INIMAGE"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{version}");
            let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
            assert_eq!(ct, "image/png", "{version}");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            assert!(body.starts_with(b"BLANK"), "{version}: expected encoder fallthrough");
            let mut w = [0u8; 4];
            w.copy_from_slice(&body[5..9]);
            assert_eq!(u32::from_le_bytes(w), 64, "{version}");
        }
    }

    #[tokio::test]
    async fn wms_exceptions_blank_returns_200_image_per_version() {
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetMap",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "16"),
                    ("height", "16"),
                    ("format", "image/png"),
                    ("exceptions", "BLANK"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{version}");
            let ct = resp.headers().get(header::CONTENT_TYPE).cloned().unwrap();
            assert_eq!(ct, "image/png", "{version}");
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            assert!(body.starts_with(b"BLANK"), "{version}: expected encoder fallthrough");
            let mut w = [0u8; 4];
            w.copy_from_slice(&body[5..9]);
            assert_eq!(u32::from_le_bytes(w), 16, "{version}");
        }
    }

    #[tokio::test]
    async fn wms_exceptions_xml_returns_service_exception_per_version() {
        // sanity inverse of the blank test: explicit exceptions=XML still
        // produces a 503 ServiceExceptionReport tagged with the right version.
        for version in WMS_VERSIONS {
            let app = empty_router();
            let uri = wms_uri(
                version,
                "GetMap",
                &[
                    ("layers", "a"),
                    ("styles", ""),
                    ("crs", "EPSG:25832"),
                    ("bbox", "0,0,10,10"),
                    ("width", "16"),
                    ("height", "16"),
                    ("format", "image/png"),
                    ("exceptions", "XML"),
                ],
            );
            let resp = app
                .oneshot(Request::builder().uri(uri.as_str()).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "{version}");
            let body = body_str(resp).await;
            assert!(body.contains("ServiceExceptionReport"), "{version}: {body}");
            assert!(body.contains(&format!(r#"version="{version}""#)), "{version}: {body}");
        }
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

    #[tokio::test]
    async fn wms_capabilities_reflects_swap_for_v111() {
        // counterpart to wms_capabilities_reflects_swap: verify the v111
        // handle is wired to the same ArcSwap mechanic so a manifest change
        // also rotates the 1.1.1 document.
        let metrics = Metrics::new().unwrap();
        let v111 = capabilities_handle("<caps111>v1</caps111>".into());
        let app = router(
            empty_runtime(&metrics),
            CapabilitiesBundle {
                wms: WmsCapabilitiesHandles {
                    v111: v111.clone(),
                    v130: capabilities_handle("<caps>v1</caps>".into()),
                },
                wmts: capabilities_handle("<wmtscaps/>".into()),
            },
            InterfacesConfig {
                wms: test_wms_cfg(),
                wmts: test_wmts_cfg(),
                cors: None,
            },
            metrics,
        );
        v111.store(Arc::new(CapabilitiesDoc::new("<caps111>v2</caps111>".to_owned())));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.1.1&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_str(resp).await;
        assert!(body.contains("<caps111>v2</caps111>"), "got: {body}");
    }

    #[tokio::test]
    async fn wms_capabilities_isolation_between_versions() {
        // empty_router seeds distinct stub bodies; each version must serve
        // only its own marker. locks dispatch against cross-pollination.
        let app = empty_router();
        let v111 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.1.1&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v130 = app
            .oneshot(
                Request::builder()
                    .uri("/wms?service=WMS&version=1.3.0&request=GetCapabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let b111 = body_str(v111).await;
        let b130 = body_str(v130).await;
        assert!(b111.contains("<wmscaps111/>"), "v111 body: {b111}");
        assert!(!b111.contains("<wmscaps/>"), "v111 leaked v130 body: {b111}");
        assert!(b130.contains("<wmscaps/>"), "v130 body: {b130}");
        assert!(!b130.contains("<wmscaps111/>"), "v130 leaked v111 body: {b130}");
    }
}
