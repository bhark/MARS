//! mars runtime use-case: per-request render pipeline. depends on the
//! `mars-render-port` *port*, never a renderer adapter; the bin chooses one.

#![forbid(unsafe_code)]

use std::sync::Arc;

use mars_render_port::Renderer;
use mars_store::{LocalCache, ObjectStore};
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    #[error(transparent)]
    Render(#[from] mars_render_port::RenderError),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// All ports the runtime needs.
pub struct Deps {
    pub store: Arc<dyn ObjectStore>,
    pub cache: Arc<dyn LocalCache>,
    pub renderer: Arc<dyn Renderer>,
}

/// The render plan as produced by the interface adapter (WMS / WMTS).
#[derive(Debug, Clone)]
pub struct RenderPlan {
    pub layers: Vec<LayerId>,
    pub bbox: Bbox,
    pub width: u32,
    pub height: u32,
    pub crs: CrsCode,
    pub format: ImageFormat,
}

pub struct Runtime {
    deps: Deps,
}

impl Runtime {
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Execute one render plan and return encoded image bytes. Phase 0 stub.
    pub async fn render(&self, _plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
        let _ = &self.deps;
        tracing::debug!("runtime: stub render() - Phase 0");
        Err(RuntimeError::NotImplemented {
            what: "mars-runtime::Runtime::render",
        })
    }
}
