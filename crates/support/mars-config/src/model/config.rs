use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{
    Artifacts, Compiler, Interfaces, Layer, Observability, Render, Reprojection, Scales, ServiceMeta, Source,
    StyleEntry, TileMatrixSet,
};

// support types imported so the struct fields compile
use crate::model::Cells;

/// Top-level service configuration. SPEC §5.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Service identity and capabilities metadata.
    pub service: ServiceMeta,
    /// Source database / change-feed configuration.
    pub source: Source,
    /// Artifact store and on-disk cache settings.
    pub artifacts: Artifacts,
    /// Scale-band definitions used by the compiler.
    pub scales: Scales,
    /// Per-band cell grid configuration. **Deprecated:** the page-keyed
    /// substrate does not consume cell-grid metadata; the field is accepted
    /// for backwards compatibility with existing fixtures and ignored.
    #[serde(default)]
    pub cells: Cells,
    /// External interface toggles (WMS / WMTS / final tile cache).
    pub interfaces: Interfaces,
    /// Named tile-matrix-set definitions for WMTS.
    #[serde(default)]
    pub tile_matrix_sets: BTreeMap<String, TileMatrixSet>,
    /// Reprojection allowlist.
    #[serde(default)]
    pub reprojection: Reprojection,
    /// Named styles, keyed by reference name.
    #[serde(default)]
    pub styles: BTreeMap<String, StyleEntry>,
    /// Layer definitions.
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// Observability settings.
    #[serde(default)]
    pub observability: Observability,
    /// Renderer / encoder settings.
    #[serde(default)]
    pub render: Render,
    /// Compiler settings (incremental window, etc).
    #[serde(default)]
    pub compiler: Compiler,
}
