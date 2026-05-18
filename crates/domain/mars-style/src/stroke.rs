//! stroke-side primitives: line caps/joins, blend mode, stamp gap, geom transform.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

/// Per-pass compositing operator. Mirrors mapserver's `COMPOSITE COMPOP`
/// scalar. `SourceOver` is the canonical "draw on top" default and is
/// omitted from authored YAML. The renderer maps these onto the underlying
/// rasteriser's blend-mode enum (tiny-skia today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BlendMode {
    #[default]
    SourceOver,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
}

/// Stroke-along-line marker repeat policy. Used by line/polyline strokes
/// that want to stamp a marker glyph along the path (e.g. arrow shafts).
/// mapserver maps `GAP` -> `interval_px` (negative gap is treated as
/// `|gap|`; the sign carries direction in mapserver but is not modelled
/// here) and `INITIALGAP` -> `initial_px`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrokeGap {
    /// Arc-length distance between successive marker stamps in pixels.
    pub interval_px: f32,
    /// Arc-length offset from the path's start to the first stamp.
    #[serde(default)]
    pub initial_px: f32,
}

/// Geometry transform applied at render time. Mirrors mapserver's
/// `GEOMTRANSFORM` for the vertex-extraction subset. The runtime derives a
/// synthetic point set from the input geometry and stamps `Style::marker`
/// (when set) at each derived position; line/polygon paint is suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeomTransform {
    /// First vertex of every part / ring.
    Start,
    /// Last vertex of every part / ring. For closed polygon rings this is
    /// the same coord as `Start` because rings are coord-closed.
    End,
    /// Every vertex of every part / ring.
    Vertices,
}

#[cfg(test)]
mod tests;
