//! server + interfaces configuration bundles and the axum layer constants.

use std::net::SocketAddr;
use std::time::Duration;

use mars_config::CorsConfig;
use mars_wms::{GfiTemplates, WmsConfig};
use mars_wmts::WmtsConfig;

pub(crate) const BODY_LIMIT_BYTES: usize = 1 << 20; // 1 MiB
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
}

/// Per-interface configuration bundle threaded into [`crate::router`] and
/// [`crate::serve`]. Grouping the WMS / WMTS / CORS knobs keeps both
/// entry-point signatures bounded as more interfaces land.
pub struct InterfacesConfig {
    pub wms: WmsConfig,
    pub wmts: WmtsConfig,
    pub cors: Option<CorsConfig>,
    /// Pre-parsed per-layer GFI templates. Empty when no layer carries one.
    pub gfi_templates: GfiTemplates,
}
