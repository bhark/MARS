//! geometry style + its resolved form.

use serde::{Deserialize, Serialize};

use crate::colour::{Colour, FillPaint};
use crate::marker::{MarkerShape, MarkerSymbol};
use crate::numeric::NumericField;
use crate::scaled::ScaledSize;
use crate::stroke::{BlendMode, GeomTransform, LineCap, LineJoin, StrokeGap};

/// Polygon / line / point fill+stroke style.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Style {
    #[serde(default)]
    pub fill: Option<FillPaint>,
    #[serde(default)]
    pub stroke: Option<Colour>,
    /// Stroke width in pixels. `ScaledSize` so authored widths can attenuate
    /// with the scale denom (MINWIDTH / MAXWIDTH / SYMBOLSCALEDENOM); the
    /// renderer consumes the resolved `f32` via `ResolvedStyle`.
    #[serde(default)]
    pub stroke_width: Option<ScaledSize>,
    #[serde(default)]
    pub stroke_dasharray: Option<Vec<f32>>,
    #[serde(default)]
    pub stroke_linecap: Option<LineCap>,
    #[serde(default)]
    pub stroke_linejoin: Option<LineJoin>,
    /// Point marker. Only meaningful when this style applies to a point
    /// geometry; the runtime ignores it for line/polygon dispatch.
    #[serde(default)]
    pub marker: Option<MarkerSymbol>,
    /// Style-wide alpha multiplier in `[0.0, 1.0]`. Applies to fill, stroke,
    /// marker, and label colours so partial transparency expressed at the
    /// style level composes with each paint's own colour alpha. mirrors
    /// mapserver's `COMPOSITE OPACITY <n>`.
    #[serde(default)]
    pub opacity: Option<f32>,
    /// Perpendicular stroke offset in pixels, positive = right of direction
    /// of travel. Used for parallel double-strokes (railway centrelines,
    /// road bevels). Closed paths reject the offset with a warning -
    /// self-intersection is acceptable for v1 on tight corners. mirrors
    /// mapserver's `OFFSET <x> -99`.
    #[serde(default)]
    pub stroke_offset_px: Option<f32>,
    /// Marker stamp policy along the path. Each stamp uses `Style::marker`
    /// rotated to the local tangent. mirrors mapserver's `GAP` /
    /// `INITIALGAP`.
    #[serde(default)]
    pub stroke_gap: Option<StrokeGap>,
    /// Derive a synthetic point set from the input geometry before render.
    /// `None` means "render the geometry as is". mirrors mapserver's
    /// `GEOMTRANSFORM` (start | end | vertices subset).
    #[serde(default)]
    pub geom_transform: Option<GeomTransform>,
    /// Suppress this pass when the feature's pixel-space bbox extent (the
    /// longer of width / height in pixels) falls below this threshold.
    /// Applied per-pass before the renderer is invoked. Mirrors mapserver's
    /// `MINFEATURESIZE`.
    #[serde(default)]
    pub min_feature_size_px: Option<f32>,
    /// Compositing operator for this pass. `None` means inherit the
    /// rasteriser default (source-over). mirrors mapserver's
    /// `COMPOSITE COMPOP <name>`.
    #[serde(default)]
    pub blend_mode: Option<BlendMode>,
}

impl Style {
    /// Resolve every size-like authored field against `denom` and return a
    /// renderer-facing variant with concrete pixel scalars. Attribute-sourced
    /// fields fall back to their authored base when no row is in scope.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> ResolvedStyle {
        self.resolve_with_attrs(denom, &mars_expr::NullAttributes)
    }

    /// Per-feature variant of [`Self::resolve`]. The decoder feeds the
    /// feature's attribute row when any pass references an attribute
    /// column; otherwise [`Self::resolve`] is the simpler form.
    #[must_use]
    pub fn resolve_with_attrs(&self, denom: u64, attrs: &dyn mars_expr::AttributeAccess) -> ResolvedStyle {
        ResolvedStyle {
            fill: self.fill.clone(),
            stroke: self.stroke,
            stroke_width: self.stroke_width.as_ref().map(|s| s.resolve_with_attrs(denom, attrs)),
            stroke_dasharray: self.stroke_dasharray.clone(),
            stroke_linecap: self.stroke_linecap,
            stroke_linejoin: self.stroke_linejoin,
            marker: self.marker.as_ref().map(|m| ResolvedMarker {
                shape: m.shape.clone(),
                size: m.size.resolve_with_attrs(denom, attrs),
                rotation_rad: m.angle.as_ref().and_then(|a| a.resolve(attrs)).map(f32::to_radians),
            }),
            opacity: self.opacity,
            stroke_offset_px: self.stroke_offset_px,
            stroke_gap: self.stroke_gap,
            geom_transform: self.geom_transform,
            blend_mode: self.blend_mode,
        }
    }

    /// True if any field on this style references a feature attribute. The
    /// decoder uses this to skip opening the artifact's attribute section
    /// when every pass on every class is purely static.
    #[must_use]
    pub fn needs_attributes(&self) -> bool {
        self.stroke_width.as_ref().is_some_and(ScaledSize::needs_attributes)
            || self.marker.as_ref().is_some_and(|m| {
                m.size.needs_attributes()
                    || m.angle
                        .as_ref()
                        .is_some_and(|a| matches!(a, NumericField::Attribute(_)))
            })
    }
}

/// Renderer-facing geometry style with every size-like field resolved to a
/// concrete `f32`. Produced by [`Style::resolve`] just before the renderer
/// crosses the port boundary; the renderer reads from this type so it never
/// has to learn about scale attenuation. Adding a new authored
/// [`Style`] field that needs resolving also adds a field here.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedStyle {
    pub fill: Option<FillPaint>,
    pub stroke: Option<Colour>,
    pub stroke_width: Option<f32>,
    pub stroke_dasharray: Option<Vec<f32>>,
    pub stroke_linecap: Option<LineCap>,
    pub stroke_linejoin: Option<LineJoin>,
    pub marker: Option<ResolvedMarker>,
    pub opacity: Option<f32>,
    pub stroke_offset_px: Option<f32>,
    pub stroke_gap: Option<StrokeGap>,
    pub geom_transform: Option<GeomTransform>,
    pub blend_mode: Option<BlendMode>,
}

/// Resolved marker: shape unchanged from authored form, `size` collapsed
/// to a concrete pixel value. `rotation_rad` carries an authored or
/// attribute-derived rotation; `None` defers to the renderer's default
/// orientation (zero for points, tangent for stamped-along-line markers).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMarker {
    pub shape: MarkerShape,
    pub size: f32,
    pub rotation_rad: Option<f32>,
}

#[cfg(test)]
mod tests;
