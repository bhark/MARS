//! `axum`-backed HTTP edge.
//!
//! Routes (SPEC §3.3):
//!
//! ```text
//! /wms        WMS 1.3.0
//! /wmts       WMTS 1.0.0
//! /healthz    liveness
//! /readyz     readiness (gated on a usable manifest)
//! /metrics    Prometheus scrape
//! /debug/*    optional, gated by config
//! ```
//!
//! Phase 0 ships the stub; the actual `axum::Router` lands in Phase 1.

#![forbid(unsafe_code)]

use std::sync::Arc;

use mars_runtime::Runtime;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("listen error: {0}")]
    Listen(String),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// HTTP server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: std::net::SocketAddr,
    pub debug_endpoints: bool,
}

/// Run the HTTP server until `shutdown` fires.
pub async fn serve(_cfg: ServerConfig, _runtime: Arc<Runtime>) -> Result<(), HttpError> {
    tracing::info!("http: stub serve() - Phase 0");
    Err(HttpError::NotImplemented {
        what: "mars-http::serve",
    })
}
