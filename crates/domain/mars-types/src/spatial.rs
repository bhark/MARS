//! spatial-index addressing: page id, decimation level, hilbert key, page key,
//! page entry, per-layer page sidecar entries.

use serde::{Deserialize, Serialize};

use crate::bbox::Bbox;
use crate::content::ContentHash;
use crate::ids::{ArtifactKey, ArtifactKeyError, BindingId, LayerId, is_safe_segment};

/// Identifier of a single page within a `(binding, level)` slice. wider than
/// strictly needed (page counts top out around the low thousands) so that the
/// 16-char lower-hex form serialised into object keys is comfortably stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageId(pub u64);

impl PageId {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for PageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // 16-hex chars, lower case. matches the `p{page_hex}` segment in keys.
        write!(f, "{:016x}", self.0)
    }
}

/// Decimation level of a binding's page artifacts. 0 = native fidelity;
/// higher numbers = coarser. u8 dwarfs the handful of levels actually used,
/// but keeps the on-disk representation stable next to other page metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DecimationLevel(pub u8);

impl DecimationLevel {
    #[must_use]
    pub const fn new(level: u8) -> Self {
        Self(level)
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl core::fmt::Display for DecimationLevel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Hilbert curve key over a binding's `combined_bbox` extent (32-bit per axis,
/// packed into a u64). defines the spatial sort order of pages within a
/// `(binding, level)` slice; range tables on `LevelMetadata` store inclusive
/// `(lo, hi)` pairs of these for binary-search lookup at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HilbertKey(pub u64);

impl HilbertKey {
    #[must_use]
    pub const fn new(key: u64) -> Self {
        Self(key)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// smallest key on the curve. useful as a sentinel in range comparisons.
    #[must_use]
    pub const fn min() -> Self {
        Self(u64::MIN)
    }

    /// largest key on the curve. useful as a sentinel in range comparisons.
    #[must_use]
    pub const fn max() -> Self {
        Self(u64::MAX)
    }
}

impl core::fmt::Display for HilbertKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// addresses a single page within `(binding, level)`. the `object_key`
/// helper renders the page's on-disk key shape `bnd/{binding}/L{level}/
/// p{page_hex}/{hash}.mars`; consumers never assemble keys by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageKey {
    pub binding_id: BindingId,
    pub level: DecimationLevel,
    pub page_id: PageId,
}

impl PageKey {
    /// build the canonical object-store key for the page artifact identified
    /// by this `PageKey` and the given content hash.
    pub fn object_key(&self, hash: &ContentHash) -> Result<ArtifactKey, ArtifactKeyError> {
        let binding_s = self.binding_id.as_str();
        if !is_safe_segment(binding_s) {
            return Err(ArtifactKeyError::Malformed {
                key: format!("bnd/{binding_s}/..."),
            });
        }
        Ok(ArtifactKey::new(format!(
            "bnd/{binding_s}/L{lvl}/p{pid}/{hex}.mars",
            lvl = self.level.get(),
            pid = self.page_id,
            hex = hash.to_hex(),
        )))
    }
}

/// manifest-level summary of one page artifact within `(binding, level)`.
/// `pages` on the manifest is sorted by `(binding_id, level, hilbert_range.0)`
/// so that the slice-scan lookup at render time is a binary search plus a
/// bounded linear scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageEntry {
    pub key: PageKey,
    pub content_hash: ContentHash,
    pub spatial_bbox: Bbox,
    /// inclusive `(lo, hi)` Hilbert key range covered by this page.
    pub hilbert_range: (HilbertKey, HilbertKey),
    pub feature_count: u64,
    pub size_bytes: u64,
}

/// kind of per-layer page sidecar artifact. class sidecars carry
/// `ClassAssignment` + `StyleRefs`; label sidecars carry `LabelCandidates`.
/// stored separately so a style-only change rewrites class sidecars without
/// touching page artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayerSidecarKind {
    Class,
    Label,
}

impl LayerSidecarKind {
    /// object-store prefix segment for sidecars of this kind.
    #[must_use]
    pub const fn key_prefix(self) -> &'static str {
        match self {
            Self::Class => "cls",
            Self::Label => "lbl",
        }
    }
}

/// manifest-level summary of one per-layer page sidecar artifact.
/// `object_key` renders the canonical `cls/...` or `lbl/...` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerSidecarEntry {
    pub layer_id: LayerId,
    pub page_key: PageKey,
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    pub kind: LayerSidecarKind,
}

impl LayerSidecarEntry {
    /// build the canonical object-store key for this sidecar artifact:
    /// `{cls|lbl}/{layer}/{binding}/L{level}/p{page_hex}/{hash}.mars`.
    pub fn object_key(&self) -> Result<ArtifactKey, ArtifactKeyError> {
        let prefix = self.kind.key_prefix();
        let layer_s = self.layer_id.as_str();
        let binding_s = self.page_key.binding_id.as_str();
        if !is_safe_segment(layer_s) || !is_safe_segment(binding_s) {
            return Err(ArtifactKeyError::Malformed {
                key: format!("{prefix}/{layer_s}/{binding_s}/..."),
            });
        }
        Ok(ArtifactKey::new(format!(
            "{prefix}/{layer_s}/{binding_s}/L{lvl}/p{pid}/{hex}.mars",
            lvl = self.page_key.level.get(),
            pid = self.page_key.page_id,
            hex = self.content_hash.to_hex(),
        )))
    }
}

#[cfg(test)]
mod tests;
