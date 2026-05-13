//! rebalance stage 4: merge the rebuild outcome into the prior manifest.
//! preserves `prior.source_version`; rebalance does not advance it.

use mars_types::Manifest;

use crate::render::RebuildOutcome;
use crate::stages::shared::merge::merge_manifest;

pub(crate) fn run(prior: &Manifest, outcome: &RebuildOutcome) -> Manifest {
    let next_version = prior.version + 1;
    merge_manifest(prior, outcome, next_version, prior.source_version.clone())
}
