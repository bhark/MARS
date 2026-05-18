//! label style, halo, anchor, placement, polygon strategy, survival policy.

use serde::{Deserialize, Serialize};

use crate::colour::Colour;
use crate::numeric::NumericField;
use crate::scaled::ScaledSize;

/// Label-typed style.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelStyle {
    pub font_family: String,
    /// Font size in pixels. `ScaledSize` so authored sizes can attenuate
    /// with the scale denom (LABEL.MINSIZE / MAXSIZE / SYMBOLSCALEDENOM);
    /// the renderer consumes the resolved `f32` via `ResolvedLabelStyle`.
    pub font_size: ScaledSize,
    pub fill: Colour,
    #[serde(default)]
    pub halo: Option<Halo>,
    // u16 to match the artifact wire format. accepting i32 here would silently
    // truncate at emit time (LabelCandidate::priority is u16); reject out-of
    // range values at config-load instead.
    #[serde(default)]
    pub priority: u16,
    /// Minimum spacing between this label's bbox and every other placed
    /// label's bbox, in pixels. Inflates the collision footprint; the
    /// larger of the two neighbours' min_distance wins per pair. Mirrors
    /// mapserver's `MINDISTANCE` (post-7.2 pixel semantics).
    #[serde(default)]
    pub min_distance: f32,
    /// Anchor keyword positioning the bbox relative to the geometry's
    /// representative point. `Auto` defers to the collision pass which
    /// walks the eight perimeter positions in mapserver order. Mirrors
    /// mapserver's `POSITION`.
    #[serde(default)]
    pub position: AnchorPosition,
    /// Offset in pixels applied after `position`. Canvas-frame for
    /// axis-aligned labels, label-local frame (rotates with the run) for
    /// labels with a non-zero angle. Mirrors mapserver's `OFFSET dx dy`.
    #[serde(default)]
    pub offset_px: (f32, f32),
    /// Label rotation in degrees, counter-clockwise. `None` defers to the
    /// placement-derived angle (zero for points/polygons, tangent for lines).
    /// `Some(NumericField::Static)` is a fixed rotation; `Some(Attribute)`
    /// sources the angle from a per-feature column at render time. Mirrors
    /// mapserver's `ANGLE <deg>` / `ANGLE [col]`.
    #[serde(default, alias = "angle_deg")]
    pub angle: Option<NumericField>,
    /// When `false`, drop labels whose bbox extends past the canvas edge.
    /// Defaults to `false` to match mapserver's `PARTIALS` default.
    #[serde(default)]
    pub partials: bool,
    /// Skip the collision pass for this label - it is always placed, and
    /// remains a collision obstacle for lower-priority labels behind it.
    /// Mirrors mapserver's `FORCE`.
    #[serde(default)]
    pub force: bool,
}

impl LabelStyle {
    /// Resolve the authored font size against `denom` and return a
    /// renderer-facing variant. Attribute-sourced fields fall back to their
    /// authored base when no row is in scope.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> ResolvedLabelStyle {
        self.resolve_with_attrs(denom, &mars_expr::NullAttributes)
    }

    /// Per-feature variant of [`Self::resolve`].
    #[must_use]
    pub fn resolve_with_attrs(&self, denom: u64, attrs: &dyn mars_expr::AttributeAccess) -> ResolvedLabelStyle {
        ResolvedLabelStyle {
            font_family: self.font_family.clone(),
            font_size: self.font_size.resolve_with_attrs(denom, attrs),
            fill: self.fill,
            halo: self.halo.clone(),
            priority: self.priority,
            min_distance: self.min_distance,
            position: self.position,
            offset_px: self.offset_px,
            angle_deg: self.angle.as_ref().and_then(|a| a.resolve(attrs)),
            partials: self.partials,
            force: self.force,
        }
    }

    /// True if any field on this label style references a feature
    /// attribute. The label decoder uses this to gate the attribute-section
    /// open.
    #[must_use]
    pub fn needs_attributes(&self) -> bool {
        self.font_size.needs_attributes()
            || self
                .angle
                .as_ref()
                .is_some_and(|a| matches!(a, NumericField::Attribute(_)))
    }
}

/// Renderer-facing label style with the font size resolved to a concrete
/// `f32`. Produced by [`LabelStyle::resolve`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLabelStyle {
    pub font_family: String,
    pub font_size: f32,
    pub fill: Colour,
    pub halo: Option<Halo>,
    pub priority: u16,
    pub min_distance: f32,
    pub position: AnchorPosition,
    pub offset_px: (f32, f32),
    pub angle_deg: Option<f32>,
    pub partials: bool,
    pub force: bool,
}

/// Anchor position keyword for a label bbox. Names where the geometry's
/// representative point sits on the label's bbox: `Uc` (upper-centre)
/// anchors the bbox's top-centre to the point, so the label appears below.
/// `Auto` defers selection to the collision pass which tries the eight
/// perimeter positions in mapserver order. Mirrors mapserver's `POSITION`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnchorPosition {
    Ul,
    Uc,
    Ur,
    Cl,
    Cc,
    Cr,
    Ll,
    Lc,
    Lr,
    #[default]
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Halo {
    // accept either `colour` or `color`; examples use the US spelling.
    #[serde(alias = "color")]
    pub colour: Colour,
    pub width: f32,
}

/// Label placement strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Placement {
    /// Single-anchor placement at the geometry's representative point.
    Point,
    /// Repeated placement along a line at fixed arc-length intervals.
    Line {
        /// Repeat distance in source-CRS units (metres in projected CRSs).
        #[serde(default = "Placement::default_repeat_m")]
        repeat_m: f64,
        /// Reject candidates whose tangent rotates by more than this across
        /// the label's footprint, in degrees.
        #[serde(default = "Placement::default_max_angle_delta_deg")]
        max_angle_delta_deg: f32,
        /// How to orient labels along the line. `Auto` rotates the whole
        /// run as a block at the sample's local tangent; `Follow` rotates
        /// each glyph to its own local tangent. Mirrors mapserver's
        /// `ANGLE AUTO` vs `ANGLE FOLLOW`.
        #[serde(default)]
        angle_mode: LineAngleMode,
    },
    /// Single-anchor placement inside a polygon.
    Polygon {
        /// Anchor selection strategy.
        #[serde(default)]
        strategy: PolygonStrategy,
    },
}

impl Placement {
    pub(crate) const fn default_repeat_m() -> f64 {
        250.0
    }
    pub(crate) const fn default_max_angle_delta_deg() -> f32 {
        25.0
    }
}

/// How a `Placement::Line` orients each placed label relative to the line's
/// local tangent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LineAngleMode {
    /// Rotate the whole run as a single block at the sample's local
    /// tangent. Cheap; mirrors mapserver's `ANGLE AUTO`.
    #[default]
    Auto,
    /// Rotate each glyph to its own local tangent so the run curves with
    /// the line. Mirrors mapserver's `ANGLE FOLLOW`.
    Follow,
}

/// Polygon-label anchor strategy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolygonStrategy {
    /// Pole-of-inaccessibility (Mapbox polylabel): iterative interior-point
    /// search. Always lands inside the polygon, even on L-shapes, donuts, and
    /// concave geometry. Default for beta credibility.
    #[default]
    #[serde(alias = "inner_skeleton")] // one-release migration from the v1.0 placeholder name
    Polylabel,
    /// True area-weighted polygon centroid (shoelace). Cheaper than polylabel,
    /// but can land outside concave polygons.
    Centroid,
}

/// Per-layer label-survival policy across decimation levels.
/// at low zoom we may prune a feature's geometry but still want its label. The
/// default `Independent` keeps the label candidate alive even when geometry is
/// dropped at this level (prevents the floating town-name regression).
/// `FollowGeometry` is the strict mode for layers where a label without its
/// geometry is meaningless.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LabelSurvival {
    /// Label retained at this level regardless of geometry pruning.
    #[default]
    Independent,
    /// Label dropped if the underlying geometry is pruned at this level.
    FollowGeometry,
}

#[cfg(test)]
mod tests;
