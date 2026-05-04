//! mars compiler use-case: change-feed → dirty cells → batch → rebuild
//! artifacts → publish manifest. all wiring is via traits; no concrete adapter
//! is named here.

#![forbid(unsafe_code)]

use std::sync::Arc;

use mars_source::{ChangeFeed, Source};
use mars_store::{ManifestPublisher, ObjectStore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum CompilerError {
    #[error(transparent)]
    Source(#[from] mars_source::SourceError),
    #[error(transparent)]
    Store(#[from] mars_store::StoreError),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// All ports the compiler depends on, bundled for easy composition by the bin.
pub struct Deps {
    pub source: Arc<dyn Source>,
    pub change_feed: Arc<dyn ChangeFeed>,
    pub store: Arc<dyn ObjectStore>,
    pub manifest: Arc<dyn ManifestPublisher>,
}

/// The compiler service. `run` returns when the cancellation token fires.
pub struct Compiler {
    deps: Deps,
}

impl Compiler {
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        Self { deps }
    }

    /// Run the compiler loop. Phase 0 returns `NotImplemented` so the bin can
    /// already wire composition without a real source.
    pub async fn run(&self, _shutdown: CancellationToken) -> Result<(), CompilerError> {
        let _ = &self.deps;
        tracing::info!("compiler: stub run() invoked; returning NotImplemented (Phase 0)");
        Err(CompilerError::NotImplemented {
            what: "mars-compiler::Compiler::run",
        })
    }
}
