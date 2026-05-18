use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::model::{
    Artifacts, Compiler, Interfaces, Layer, Observability, Render, Reprojection, Scales, ServiceMeta, Source,
    StyleEntry, TileMatrixSet,
};

// support types imported so the struct fields compile
use crate::model::Cells;

/// Top-level service configuration. Wire-format target for `Config::load` and
/// the runtime-facing struct consumed by `bin/mars`. After Phase A of the
/// definition/deployment split, `Config` is also the composition target of
/// `compose(RenderDefinition, Deployment)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Service identity and capabilities metadata.
    pub service: ServiceMeta,
    /// Configured data sources. Each carries a unique id; per-layer bindings
    /// reference it to pick the backend that feeds them. Multiple sources of
    /// different (or the same) kind may coexist - e.g. a postgis source for
    /// CDC-friendly tables alongside a vectorfile source pulling FlatGeobuf
    /// from an object store.
    #[serde(default)]
    pub sources: Vec<Source>,
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

/// Portable, env-agnostic "what to render" half of the service definition.
/// Carries the fields a service author edits to publish layers: identity,
/// scales, interface toggles, tile-matrix-sets, styles, layers (with bindings
/// that name sources by logical id only). Deployment-shaped state (DSNs,
/// stores, observability, runtime tuning) lives on [`Deployment`].
///
/// Composed back into a runnable [`Config`] via [`compose`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderDefinition {
    /// Service identity and capabilities metadata.
    pub service: ServiceMeta,
    /// Scale-band definitions used by the compiler.
    pub scales: Scales,
    /// External interface toggles (WMS / WMTS / final tile cache).
    #[serde(default)]
    pub interfaces: Interfaces,
    /// Named tile-matrix-set definitions for WMTS.
    #[serde(default)]
    pub tile_matrix_sets: BTreeMap<String, TileMatrixSet>,
    /// Service-side narrowing of the deployment reprojection allowlist. Empty
    /// means "fall back to deployment default" at [`compose`] time.
    #[serde(default)]
    pub reprojection: Reprojection,
    /// Named styles, keyed by reference name.
    #[serde(default)]
    pub styles: BTreeMap<String, StyleEntry>,
    /// Layer definitions. Bindings name sources by logical id only.
    #[serde(default)]
    pub layers: Vec<Layer>,
}

/// "Where / how to run it" half of the service definition. Carries env-shaped
/// state: source DSNs, artifact store endpoint, cache config, observability,
/// runtime/compiler tuning, and the cluster-default reprojection allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    /// Configured data sources (DSN, change_feed, bootstrap, pool tuning,
    /// vectorfile cache_dir/polling).
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Artifact store and on-disk cache settings.
    pub artifacts: Artifacts,
    /// Observability settings.
    #[serde(default)]
    pub observability: Observability,
    /// Renderer / encoder settings.
    #[serde(default)]
    pub render: Render,
    /// Compiler settings (incremental window, etc).
    #[serde(default)]
    pub compiler: Compiler,
    /// Cluster-default reprojection allowlist. Narrowed by
    /// [`RenderDefinition::reprojection`] at [`compose`] time via intersection.
    #[serde(default)]
    pub reprojection: Reprojection,
}

/// Compose a runnable [`Config`] from a render definition and a deployment.
///
/// Total partition: every [`Config`] field is sourced from exactly one of
/// `def` or `dep`, with one cross-side merge - the reprojection allowlist
/// is intersected ([`intersect_reprojection`]). The deprecated `cells` field
/// is defaulted; it is ignored by the runtime.
#[must_use]
pub fn compose(def: RenderDefinition, dep: Deployment) -> Config {
    Config {
        service: def.service,
        sources: dep.sources,
        artifacts: dep.artifacts,
        scales: def.scales,
        cells: Cells::default(),
        interfaces: def.interfaces,
        tile_matrix_sets: def.tile_matrix_sets,
        reprojection: intersect_reprojection(&dep.reprojection, &def.reprojection),
        styles: def.styles,
        layers: def.layers,
        observability: dep.observability,
        render: dep.render,
        compiler: dep.compiler,
    }
}

/// Intersect the deployment (cluster default) allowlist with the render
/// definition (service narrowing) allowlist. Empty service allowlist falls
/// back to the cluster default. Non-empty service allowlist is intersected
/// element-wise with the cluster allowlist; order follows the service-side
/// declaration so authors see their preferred ordering preserved.
fn intersect_reprojection(deployment: &Reprojection, definition: &Reprojection) -> Reprojection {
    if definition.allowlist.is_empty() {
        return deployment.clone();
    }
    let allowed: HashSet<&str> = deployment.allowlist.iter().map(|c| c.as_str()).collect();
    Reprojection {
        allowlist: definition
            .allowlist
            .iter()
            .filter(|c| allowed.contains(c.as_str()))
            .cloned()
            .collect(),
    }
}

impl RenderDefinition {
    /// Validate the render-side fields and resolve derived state (band
    /// routing) on the layers. Excludes any check that depends on a `Source`
    /// catalog - those run on the composed [`Config`] via [`crate::validate`].
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        crate::validate::validate_render_definition(self)
    }
}

impl Deployment {
    /// Validate the deployment-side fields. Excludes any check that depends
    /// on the render definition (layers, styles, scales) - those run on the
    /// composed [`Config`] via [`crate::validate`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        crate::validate::validate_deployment(self)
    }
}
