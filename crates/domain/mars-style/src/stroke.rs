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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn blend_mode_serializes_kebab_case() {
        let yaml = serde_yaml_ng::to_string(&BlendMode::SourceOver).unwrap();
        assert!(yaml.trim() == "source-over");
        let parsed: BlendMode = serde_yaml_ng::from_str("source-over").unwrap();
        assert_eq!(parsed, BlendMode::SourceOver);
    }

    #[test]
    fn stroke_gap_initial_defaults_to_zero() {
        let g: StrokeGap = serde_yaml_ng::from_str("interval_px: 8.0\n").unwrap();
        assert!((g.interval_px - 8.0).abs() < f32::EPSILON);
        assert!(g.initial_px.abs() < f32::EPSILON);
    }

    #[test]
    fn geom_transform_wire_form_is_snake_case() {
        for (variant, wire) in [
            (GeomTransform::Start, "start"),
            (GeomTransform::End, "end"),
            (GeomTransform::Vertices, "vertices"),
        ] {
            let out = serde_yaml_ng::to_string(&variant).unwrap();
            assert_eq!(out.trim(), wire);
            let back: GeomTransform = serde_yaml_ng::from_str(wire).unwrap();
            assert_eq!(back, variant);
        }
    }
}
