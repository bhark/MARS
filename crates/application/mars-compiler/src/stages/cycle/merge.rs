//! cycle stage 5: merge the rebuild outcome into the prior manifest with
//! the latest-batch `source_version` threaded through.

use mars_types::Manifest;

use crate::render::RebuildOutcome;
use crate::stages::shared::merge::merge_manifest;

pub(crate) fn run(prior: &Manifest, outcome: &RebuildOutcome, last_source_version: Option<String>) -> Manifest {
    let next_version = prior.version + 1;
    merge_manifest(prior, outcome, next_version, last_source_version)
}
