use mars_style::LabelSurvival;
use mars_types::{Bbox, LayerId};
use serde::{Deserialize, Serialize};

mod binding;
mod class;
mod label;
mod raster;

pub use binding::*;
pub use class::*;
pub use label::*;
pub use raster::*;

use super::ows::{LayerOws, ServiceOp};
use super::wms::LayerWms;

/// Layer definition. Carries identity, geometry binding and rendering data;
/// WMS-protocol metadata and per-operation gating live on [`Self::wms`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    /// Stable layer identifier.
    pub name: LayerId,
    /// Human-readable layer title.
    #[serde(default)]
    pub title: String,
    /// Long-form abstract.
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    /// Geometry kind (`polygon`, `line`, `point`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Layer-wide scale window.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Optional flat group string.
    #[serde(default)]
    pub group: Option<String>,
    /// Optional layer-wide bounding-box constraint.
    #[serde(default)]
    pub bbox: Option<Bbox>,
    /// One or more source bindings. Required for vector layers; raster layers
    /// must leave this empty (their tile source lives under `raster:`).
    #[serde(default)]
    pub sources: Vec<SourceBinding>,
    /// Class list, top-down first-match-wins.
    #[serde(default)]
    pub classes: Vec<Class>,
    /// Optional label declaration.
    #[serde(default)]
    pub label: Option<LayerLabel>,
    /// Label-survival policy across decimation levels. Default `Independent`
    /// (label retained even when geometry is pruned at the level).
    #[serde(default)]
    pub label_survival: LabelSurvival,
    /// Raster layer spec. Required when `kind == "raster"`; rejected
    /// otherwise. Mutually exclusive with `sources`, `classes`, and `label`.
    #[serde(default)]
    pub raster: Option<RasterLayerSpec>,
    /// Per-layer WMS metadata and request gating. Defaults to an empty,
    /// permissive block when omitted.
    #[serde(default)]
    pub wms: LayerWms,
    /// Per-layer OWS metadata + cross-protocol request gating. Defaults to
    /// an empty, permissive block when omitted.
    #[serde(default)]
    pub ows: LayerOws,
    /// Optional per-feature template body for GetFeatureInfo responses.
    /// When set, every hit in this layer is rendered with the template and
    /// the rendered string replaces the default key/value table in
    /// `text/plain` and `text/html` (and surfaces as a `"rendered"` field
    /// alongside `attrs` in `application/json`). Mirrors MapServer
    /// `TEMPLATE "path.html"`. Identifier syntax is `{attr}`, matching the
    /// label-text template parser ([`mars_expr::parse_template`]).
    #[serde(default)]
    pub template: Option<String>,
}

impl Layer {
    /// Resolved gating decision for any OWS-family operation. The explicit
    /// `ows.request_gating` map wins. When it is silent, every op default-
    /// allows except `WmsGetFeatureInfo`, which falls back to the legacy
    /// [`LayerWms::enable_get_feature_info`] opt-in (GFI's spec-default is
    /// deny).
    #[must_use]
    pub fn permits_op(&self, op: ServiceOp) -> bool {
        if let Some(b) = self.ows.request_gating.get(&op).copied() {
            return b;
        }
        match op {
            ServiceOp::WmsGetFeatureInfo => self.wms.enable_get_feature_info,
            _ => true,
        }
    }
}

/// Half-open scale window with denominator bounds. Shared by [`Layer`],
/// [`Class`] and [`SourceBinding`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScaleWindow {
    /// Inclusive lower bound on scale denominator.
    #[serde(default)]
    pub min: Option<u64>,
    /// Exclusive upper bound on scale denominator.
    #[serde(default)]
    pub max: Option<u64>,
}

#[cfg(test)]
mod tests;
