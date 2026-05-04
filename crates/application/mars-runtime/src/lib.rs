//! mars runtime use-case: per-request render pipeline. depends on the
//! `mars-render-port` *port*, never a renderer adapter; the bin chooses one.

#![forbid(unsafe_code)]

mod draw;
mod fetch;
pub mod key;
mod plan;
pub mod state;

use std::sync::Arc;

use mars_render_port::{Canvas, Renderer};
use mars_store::{LocalCache, ObjectStore};
use mars_types::{Bbox, CrsCode, ImageFormat, LayerId};

pub use plan::denom_from_plan;
pub use state::RuntimeState;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    #[error(transparent)]
    Render(#[from] mars_render_port::RenderError),
    #[error(transparent)]
    Artifact(#[from] mars_artifact::ArtifactError),
    #[error(transparent)]
    Grid(#[from] mars_grid::GridError),
    #[error("plan CRS '{requested}' is not the canonical CRS; reprojection deferred to phase 1")]
    CrsNotCanonical { requested: String },
    #[error("manifest entry missing for layer '{layer}' band '{band}' cell {cell:?}")]
    ManifestEntryMissing {
        layer: String,
        band: String,
        cell: (i64, i64),
    },
    #[error("source artifact missing for collection '{collection}' band '{band}' cell {cell:?}")]
    SourceMissing {
        collection: String,
        band: String,
        cell: (i64, i64),
    },
    #[error("malformed manifest key: {0}")]
    BadKey(String),
    #[error("config error: {0}")]
    Config(String),
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
    state: Arc<RuntimeState>,
    deps: Deps,
}

impl Runtime {
    /// Compose a runtime from a pre-built state snapshot and the dep set.
    #[must_use]
    pub fn from_state(state: Arc<RuntimeState>, deps: Deps) -> Self {
        Self { state, deps }
    }

    /// Borrow the active state snapshot.
    #[must_use]
    pub fn state(&self) -> &RuntimeState {
        &self.state
    }

    /// Execute one render plan and return encoded image bytes.
    pub async fn render(&self, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
        if plan.crs != self.state.canonical_crs {
            return Err(RuntimeError::CrsNotCanonical {
                requested: plan.crs.to_string(),
            });
        }

        let tasks = plan::resolve(plan, &self.state)?;
        let viewport = draw::Viewport {
            bbox: plan.bbox,
            width: plan.width,
            height: plan.height,
        };

        let mut ops = Vec::new();
        for task in &tasks {
            let layer_art = fetch::fetch_layer(
                &self.state,
                self.deps.cache.as_ref(),
                self.deps.store.as_ref(),
                &task.layer,
                &task.cell,
            )
            .await?;
            let source_ref = layer_art.source_ref().cloned().ok_or_else(|| {
                RuntimeError::Config(format!(
                    "layer artifact '{}' is missing source_ref footer",
                    task.layer
                ))
            })?;
            let source_cell = mars_types::Cell {
                band: mars_types::ScaleBand::new(source_ref.band.clone()),
                x: source_ref.cell_x,
                y: source_ref.cell_y,
            };
            let source_art = fetch::fetch_source(
                &self.state,
                self.deps.cache.as_ref(),
                self.deps.store.as_ref(),
                &source_ref.collection,
                &source_cell,
            )
            .await?;

            draw::emit_layer_cell(
                &source_art,
                &layer_art,
                &self.state.stylesheet,
                viewport,
                &mut ops,
            )?;
        }

        let canvas = Canvas {
            width: plan.width,
            height: plan.height,
            background: None,
        };
        let bytes = self.deps.renderer.render(canvas, &ops, plan.format).await?;
        Ok(bytes)
    }
}
