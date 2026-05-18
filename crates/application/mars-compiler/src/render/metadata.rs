//! Small pure helpers shared by the render submodules: level-metadata
//! recomputation after a rebuild pass, an empty-level constructor, the
//! page-membership sidecar object-key builder, and the source→artifact
//! attribute-value bridge.

use mars_artifact::AttrValue as ArtAttrValue;
use mars_source::AttrValue;
use mars_types::{ArtifactKey, BindingId, ContentHash, HilbertKey, LevelMetadata, PageEntry, PageId};

use crate::CompilerError;
use crate::plan::LevelPlan;

/// Recompute level metadata after pages were replaced or dropped. Pure;
/// runs at the cycle entry point after all rebuilds finish, against the
/// merged page list. Exposed here rather than at the cycle entry point
/// because it is the natural complement to [`super::rebuild_pages`].
#[must_use]
pub fn recompute_level_metadata(prior: &LevelMetadata, pages: &[PageEntry], binding_id: &BindingId) -> LevelMetadata {
    let mut ranges: Vec<(HilbertKey, HilbertKey, PageId)> = pages
        .iter()
        .filter(|p| p.key.binding_id == *binding_id && p.key.level == prior.level)
        .map(|p| (p.hilbert_range.0, p.hilbert_range.1, p.key.page_id))
        .collect();
    ranges.sort_by_key(|r| r.0);
    LevelMetadata {
        level: prior.level,
        vertex_tolerance_m: prior.vertex_tolerance_m,
        geometry_min_size_m: prior.geometry_min_size_m,
        label_min_priority: prior.label_min_priority,
        page_count: ranges.len() as u32,
        hilbert_range_table: ranges,
    }
}

pub(crate) fn empty_level_metadata(level: &LevelPlan) -> LevelMetadata {
    LevelMetadata {
        level: level.level,
        vertex_tolerance_m: level.vertex_tolerance_m,
        geometry_min_size_m: level.geometry_min_size_m,
        label_min_priority: level.label_min_priority,
        page_count: 0,
        hilbert_range_table: Vec::new(),
    }
}

pub(crate) fn membership_sidecar_object_key(binding: &str, hash: &ContentHash) -> Result<ArtifactKey, CompilerError> {
    if binding.contains('/') || binding.contains('\0') {
        return Err(CompilerError::InvalidBindingId {
            binding: binding.to_string(),
        });
    }
    Ok(ArtifactKey::new(format!(
        "bnd/{binding}/sidecar/{hex}.pmsc",
        hex = hash.to_hex()
    )))
}

pub(crate) fn attr_value_to_artifact(v: &AttrValue) -> ArtAttrValue {
    match v {
        AttrValue::Null => ArtAttrValue::Null,
        AttrValue::Bool(b) => ArtAttrValue::Bool(*b),
        AttrValue::Int(i) => ArtAttrValue::Int(*i),
        AttrValue::Float(f) => ArtAttrValue::Float(*f),
        AttrValue::String(s) => ArtAttrValue::String(s.clone()),
    }
}

#[cfg(test)]
mod tests;
