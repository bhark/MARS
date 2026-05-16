//! shared per-request state and capabilities-document handles.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arc_swap::ArcSwap;
use mars_observability::Metrics;
use mars_runtime::Runtime;
use mars_wms::{GfiTemplates, WmsConfig, WmsVersion};
use mars_wmts::WmtsConfig;

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
    pub(crate) runtime: Arc<Runtime>,
    pub(crate) wms_capabilities: WmsCapabilitiesHandles,
    pub(crate) wmts_capabilities: CapabilitiesHandle,
    pub(crate) wms_cfg: Arc<WmsConfig>,
    pub(crate) wmts_cfg: Arc<WmtsConfig>,
    pub(crate) gfi_templates: Arc<GfiTemplates>,
    pub(crate) metrics: Metrics,
    pub(crate) request_counter: Arc<AtomicU64>,
}

/// strong validator for capability documents. blake3-truncated to 64 bits;
/// collision-safe for the handful of documents we serve.
pub(crate) fn etag_for(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    format!("\"{}\"", &hash.to_hex().as_str()[..16])
}
