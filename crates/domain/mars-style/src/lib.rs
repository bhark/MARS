//! MARS style model. SPEC §5.4 / §5.5 - a small fixed vocabulary close to SVG.
//!
//! No rendering happens here; the renderer adapter consumes the compiled form.

#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, thiserror::Error)]
pub enum StyleError {
    #[error("invalid colour: {0}")]
    InvalidColour(String),
    #[error("invalid style: {0}")]
    Invalid(String),
}

/// Hex colour as parsed from `#rrggbb` or `#rrggbbaa`. Serialises back to the
/// canonical `#rrggbb` form when alpha is opaque, `#rrggbbaa` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Colour {
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xff }
    }

    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

impl FromStr for Colour {
    type Err = StyleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = s
            .strip_prefix('#')
            .ok_or_else(|| StyleError::InvalidColour(s.to_owned()))?;
        let parse = |i: usize| -> Result<u8, StyleError> {
            let slice = raw
                .get(i..i + 2)
                .ok_or_else(|| StyleError::InvalidColour(s.to_owned()))?;
            u8::from_str_radix(slice, 16).map_err(|_| StyleError::InvalidColour(s.to_owned()))
        };
        match raw.len() {
            6 => Ok(Self::rgba(parse(0)?, parse(2)?, parse(4)?, 0xff)),
            8 => Ok(Self::rgba(parse(0)?, parse(2)?, parse(4)?, parse(6)?)),
            _ => Err(StyleError::InvalidColour(s.to_owned())),
        }
    }
}

impl fmt::Display for Colour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 0xff {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(f, "#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

impl Serialize for Colour {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

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

/// Polygon / line / point fill+stroke style.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Style {
    #[serde(default)]
    pub fill: Option<Colour>,
    #[serde(default)]
    pub stroke: Option<Colour>,
    #[serde(default)]
    pub stroke_width: Option<f32>,
    #[serde(default)]
    pub stroke_dasharray: Option<Vec<f32>>,
    #[serde(default)]
    pub stroke_linecap: Option<LineCap>,
    #[serde(default)]
    pub stroke_linejoin: Option<LineJoin>,
}

/// Label-typed style. SPEC §5.4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelStyle {
    pub font_family: String,
    pub font_size: f32,
    pub fill: Colour,
    #[serde(default)]
    pub halo: Option<Halo>,
    // u16 to match the artifact wire format. accepting i32 here would silently
    // truncate at emit time (LabelCandidate::priority is u16); reject out-of
    // range values at config-load instead.
    #[serde(default)]
    pub priority: u16,
    #[serde(default)]
    pub min_distance: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Halo {
    // accept either `colour` or `color`; SPEC examples use the US spelling.
    #[serde(alias = "color")]
    pub colour: Colour,
    pub width: f32,
}

/// Label placement strategy. SPEC §5.5.
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
    },
    /// Single-anchor placement inside a polygon.
    Polygon {
        /// Anchor selection strategy.
        #[serde(default)]
        strategy: PolygonStrategy,
    },
}

impl Placement {
    const fn default_repeat_m() -> f64 {
        250.0
    }
    const fn default_max_angle_delta_deg() -> f32 {
        25.0
    }
}

/// Polygon-label anchor strategy. SPEC §14.1.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolygonStrategy {
    /// Place the label at the polygon centroid.
    #[default]
    Centroid,
    /// Reserved for v1.1: constrained inner-skeleton sample.
    InnerSkeleton,
}

/// Layer geometry kind. Mirrors the layer `type:` field in service config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerGeomKind {
    Point,
    Line,
    Polygon,
}

impl LayerGeomKind {
    /// Parse the `type:` field of a layer.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "point" => Some(Self::Point),
            "line" => Some(Self::Line),
            "polygon" => Some(Self::Polygon),
            _ => None,
        }
    }
}

/// Default placement for a layer with no explicit `placement:` block.
/// SPEC §5.5: lines repeat at 250 m with a 25° angle gate; everything else
/// gets a single point anchor.
#[must_use]
pub fn default_placement(kind: LayerGeomKind) -> Placement {
    match kind {
        LayerGeomKind::Line => Placement::Line {
            repeat_m: 250.0,
            max_angle_delta_deg: 25.0,
        },
        LayerGeomKind::Polygon => Placement::Polygon {
            strategy: PolygonStrategy::Centroid,
        },
        LayerGeomKind::Point => Placement::Point,
    }
}

/// Compiled stylesheet, keyed by style name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stylesheet {
    #[serde(default)]
    pub geometry: std::collections::BTreeMap<String, Arc<Style>>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, LabelStyle>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn default_style_is_empty() {
        let s = Style::default();
        assert!(s.fill.is_none());
        assert!(s.stroke.is_none());
    }

    #[test]
    fn colour_parses_rrggbb() {
        let c: Colour = "#fafafa".parse().unwrap();
        assert_eq!(c, Colour::rgba(0xfa, 0xfa, 0xfa, 0xff));
    }

    #[test]
    fn colour_parses_rrggbbaa() {
        let c: Colour = "#01020380".parse().unwrap();
        assert_eq!(c, Colour::rgba(1, 2, 3, 0x80));
    }

    #[test]
    fn colour_rejects_garbage() {
        assert!("fafafa".parse::<Colour>().is_err());
        assert!("#fafaf".parse::<Colour>().is_err());
        assert!("#zzzzzz".parse::<Colour>().is_err());
    }

    #[test]
    fn colour_round_trip_serde() {
        let c = Colour::rgba(0xfa, 0xfa, 0xfa, 0xff);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"#fafafa\"");
        let back: Colour = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn colour_round_trip_with_alpha() {
        let c = Colour::rgba(1, 2, 3, 0x80);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"#01020380\"");
        let back: Colour = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn polygon_style_from_spec_example_round_trips() {
        // mirrors SPEC §5.4: a `bygning_*` polygon style.
        let json = r##"{"fill":"#fafafa","stroke":"#b4b4b4","stroke_width":0.6}"##;
        let s: Style = serde_json::from_str(json).unwrap();
        assert_eq!(s.fill.unwrap(), Colour::rgba(0xfa, 0xfa, 0xfa, 0xff));
        assert_eq!(s.stroke.unwrap(), Colour::rgba(0xb4, 0xb4, 0xb4, 0xff));
        assert!((s.stroke_width.unwrap() - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn placement_round_trips_each_variant() {
        let p: Placement = serde_yaml_ng::from_str("kind: point").unwrap();
        assert!(matches!(p, Placement::Point));

        let p: Placement = serde_yaml_ng::from_str("kind: line").unwrap();
        match p {
            Placement::Line {
                repeat_m,
                max_angle_delta_deg,
            } => {
                assert!((repeat_m - 250.0).abs() < f64::EPSILON);
                assert!((max_angle_delta_deg - 25.0).abs() < f32::EPSILON);
            }
            _ => panic!("expected line"),
        }

        let p: Placement = serde_yaml_ng::from_str("kind: line\nrepeat_m: 100\nmax_angle_delta_deg: 10").unwrap();
        match p {
            Placement::Line {
                repeat_m,
                max_angle_delta_deg,
            } => {
                assert!((repeat_m - 100.0).abs() < f64::EPSILON);
                assert!((max_angle_delta_deg - 10.0).abs() < f32::EPSILON);
            }
            _ => panic!("expected line"),
        }

        let p: Placement = serde_yaml_ng::from_str("kind: polygon").unwrap();
        assert!(matches!(
            p,
            Placement::Polygon {
                strategy: PolygonStrategy::Centroid
            }
        ));

        let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: inner_skeleton").unwrap();
        assert!(matches!(
            p,
            Placement::Polygon {
                strategy: PolygonStrategy::InnerSkeleton
            }
        ));
    }

    #[test]
    fn default_placement_picks_per_geom_kind() {
        assert!(matches!(default_placement(LayerGeomKind::Point), Placement::Point));
        assert!(matches!(
            default_placement(LayerGeomKind::Line),
            Placement::Line { repeat_m: 250.0, .. }
        ));
        assert!(matches!(
            default_placement(LayerGeomKind::Polygon),
            Placement::Polygon {
                strategy: PolygonStrategy::Centroid
            }
        ));
    }

    #[test]
    fn label_style_from_spec_example_round_trips() {
        let json = r##"{
            "font_family": "Arial",
            "font_size": 12,
            "fill": "#000000",
            "halo": { "color": "#ffffff", "width": 1.5 },
            "priority": 100,
            "min_distance": 50
        }"##;
        let l: LabelStyle = serde_json::from_str(json).unwrap();
        assert_eq!(l.font_family, "Arial");
        assert_eq!(l.fill, Colour::rgba(0, 0, 0, 0xff));
        let halo = l.halo.unwrap();
        assert_eq!(halo.colour, Colour::rgba(0xff, 0xff, 0xff, 0xff));
        assert!((halo.width - 1.5).abs() < f32::EPSILON);
    }
}
