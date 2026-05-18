//! manifest watch consumer.
//!
//! polls the manifest store, validates each candidate snapshot against the
//! active config + stylesheet, refreshes the bundled image registry, and
//! atomically swaps a fresh `RuntimeState` into the runtime. monotonicity is
//! enforced here so backwards-version snapshots never reach the hot path.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use mars_observability::reject_reason;
use mars_store::{ManifestStore, StoreError};
use mars_style::Stylesheet;

use crate::{Runtime, RuntimeError, images, state::RuntimeState};

/// idle hint for reload-loop helpers; exposed so tests can match.
#[doc(hidden)]
pub const RELOAD_IDLE_HINT: Duration = Duration::from_secs(5);

/// Consume a manifest watch stream and atomically hot-swap valid runtime states.
/// Returns when the stream ends or `shutdown` is cancelled.
pub async fn run_manifest_reload_loop(
    runtime: Arc<Runtime>,
    manifests: Arc<dyn ManifestStore>,
    config: Arc<mars_config::Config>,
    stylesheet: Stylesheet,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(), RuntimeError> {
    let mut manifests = manifests.watch().await?;

    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            n = manifests.next() => match n {
                Some(n) => n,
                None => return Ok(()),
            },
        };
        let manifest = match next {
            Ok(m) => m,
            Err(e) => {
                let label = classify_store_error(&e);
                let reason = format!("invalid snapshot: {e}");
                tracing::error!(error = %e, "manifest watch: ignoring invalid snapshot");
                runtime.deps().metrics.inc_manifest_reject(label);
                runtime.record_reject(reason);
                continue;
            }
        };

        // monotonicity: refuse anything not strictly newer than the active version.
        if let Some(current) = runtime.current_state()
            && manifest.version <= current.manifest.version
        {
            if manifest.version < current.manifest.version {
                let reason = format!(
                    "manifest version {} is older than active {}",
                    manifest.version, current.manifest.version
                );
                runtime
                    .deps()
                    .metrics
                    .inc_manifest_reject(reject_reason::BACKWARDS_VERSION);
                runtime.record_reject(reason);
            }
            continue;
        }

        let new_version = manifest.version;
        let image_artifact = manifest.image_artifact.clone();
        match RuntimeState::from_config_and_manifest(&config, stylesheet.clone(), manifest) {
            Ok(state) => {
                match images::load_from_manifest(image_artifact.as_ref(), &runtime.deps().cache, &runtime.deps().store)
                    .await
                {
                    Ok(map) => runtime.deps().images.set(map),
                    Err(e) => {
                        let reason = format!("manifest v{new_version} image_artifact load failed: {e}");
                        runtime
                            .deps()
                            .metrics
                            .inc_manifest_reject(reject_reason::VALIDATION_ERROR);
                        runtime.record_reject(reason);
                        continue;
                    }
                }
                runtime.swap_state(Arc::new(state));
                tracing::info!(version = new_version, "runtime: manifest swapped");
            }
            Err(e) => {
                let reason = format!("manifest v{new_version} rejected: {e}");
                runtime
                    .deps()
                    .metrics
                    .inc_manifest_reject(reject_reason::VALIDATION_ERROR);
                runtime.record_reject(reason);
            }
        }
    }
}

fn classify_store_error(e: &StoreError) -> &'static str {
    match e {
        StoreError::UnsupportedManifestVersion { .. } => reject_reason::UNSUPPORTED_FORMAT_VERSION,
        StoreError::HashMismatch { .. } => reject_reason::HASH_MISMATCH,
        _ => reject_reason::IO_ERROR,
    }
}

#[cfg(test)]
mod tests;
