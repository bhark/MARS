//! no-op manifest version bump.
//!
//! published when a cycle observes zero dirty bindings or when a rebalance
//! pass finds the manifest already balanced. downstream cursors advance on
//! every published version so an empty window still produces a manifest;
//! collapsing the two ad-hoc bumps that lived inline in `apply_cycle` and
//! `rebalance_locked` into one helper keeps the two paths from drifting on
//! `created_at`, `epoch`, or `source_version` handling.

use mars_types::Manifest;

/// produce the next version of `prior` with `source_version` replacing the
/// prior value. cycle passes the latest batch's `source_version`; rebalance
/// passes `prior.source_version.clone()` to preserve it explicitly.
pub(crate) fn build(prior: Manifest, source_version: Option<String>) -> Manifest {
    let next_version = prior.version + 1;
    let mut next = prior;
    next.version = next_version;
    next.epoch = next_version;
    next.source_version = source_version;
    next.created_at = std::time::SystemTime::now();
    next
}
