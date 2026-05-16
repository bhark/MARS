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

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use mars_config::CorsConfig;
use mars_observability::Metrics;
use mars_runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;

mod capabilities;
mod config;
mod errors;
mod handlers;
mod middleware;
mod state;

#[cfg(test)]
mod tests;

pub use config::{InterfacesConfig, ServerConfig};
pub use errors::*;
pub use handlers::*;
pub use middleware::*;
pub use state::{
    AppState, CapabilitiesBundle, CapabilitiesDoc, CapabilitiesHandle, WmsCapabilitiesHandles, capabilities_handle,
};

use config::{BODY_LIMIT_BYTES, REQUEST_TIMEOUT};

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("listen error: {0}")]
    Listen(String),
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
        gfi_templates,
    } = interfaces;
    let state = AppState {
        runtime,
        wms_capabilities: capabilities.wms,
        wmts_capabilities: capabilities.wmts,
        wms_cfg: Arc::new(wms_cfg),
        wmts_cfg: Arc::new(wmts_cfg),
        gfi_templates: Arc::new(gfi_templates),
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
