//! MARS style model. a small fixed vocabulary close to SVG.
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

/// Point marker symbol set. Extensible via `#[non_exhaustive]` and the tagged
/// `kind:` discriminator - future variants (e.g. `Path { svg }`, `Pixmap { uri }`)
/// land additively without breaking existing wire form.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MarkerSymbol {
    Circle {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
    Square {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
    Triangle {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
    Cross {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
    X {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
    Pin {
        #[serde(default = "MarkerSymbol::default_size")]
        size: f32,
    },
}

impl MarkerSymbol {
    const fn default_size() -> f32 {
        6.0
    }

    /// Marker bounding-box edge length in pixels. Pin is teardrop, so size
    /// is the bulb diameter; the pin extends downward by ~1.5x.
    #[must_use]
    pub const fn size(&self) -> f32 {
        match *self {
            Self::Circle { size }
            | Self::Square { size }
            | Self::Triangle { size }
            | Self::Cross { size }
            | Self::X { size }
            | Self::Pin { size } => size,
        }
    }
}

/// Polygon fill paint. `Solid` is a bare hex string on the wire; `Hatch` is a
/// tagged map. `#[non_exhaustive]` so future variants (cross-hatch, dots,
/// pixmap) land additively.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum FillPaint {
    Solid(Colour),
    Hatch {
        spacing: f32,
        angle_deg: f32,
        line_width: f32,
        colour: Colour,
    },
}

impl Serialize for FillPaint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match *self {
            // bare hex string preserves wire-format symmetry with the legacy
            // `fill: Option<Colour>` form. existing configs and goldens stay
            // diff-clean.
            Self::Solid(c) => c.serialize(s),
            Self::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            } => {
                let mut st = s.serialize_struct("Hatch", 5)?;
                st.serialize_field("kind", "hatch")?;
                st.serialize_field("spacing", &spacing)?;
                st.serialize_field("angle_deg", &angle_deg)?;
                st.serialize_field("line_width", &line_width)?;
                st.serialize_field("colour", &colour)?;
                st.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for FillPaint {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // accept either a hex string (Solid, legacy form preserved for
        // wire-format symmetry) or a tagged map (Hatch et al.).
        d.deserialize_any(FillPaintVisitor)
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FillPaintTagged {
    Solid {
        colour: Colour,
    },
    Hatch {
        spacing: f32,
        angle_deg: f32,
        line_width: f32,
        colour: Colour,
    },
}

impl From<FillPaintTagged> for FillPaint {
    fn from(t: FillPaintTagged) -> Self {
        match t {
            FillPaintTagged::Solid { colour } => Self::Solid(colour),
            FillPaintTagged::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            } => Self::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            },
        }
    }
}

struct FillPaintVisitor;

impl<'de> serde::de::Visitor<'de> for FillPaintVisitor {
    type Value = FillPaint;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a hex colour string (#rrggbb / #rrggbbaa) or a tagged map (kind: solid|hatch)")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Colour::from_str(v).map(FillPaint::Solid).map_err(E::custom)
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.visit_str(&v)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        let tagged = FillPaintTagged::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(tagged.into())
    }
}

/// Polygon / line / point fill+stroke style.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Style {
    #[serde(default)]
    pub fill: Option<FillPaint>,
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
    /// Point marker. Only meaningful when this style applies to a point
    /// geometry; the runtime ignores it for line/polygon dispatch.
    #[serde(default)]
    pub marker: Option<MarkerSymbol>,
}

/// Label-typed style.
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
/// lines repeat at 250 m with a 25° angle gate; everything else
/// gets a single point anchor.
#[must_use]
pub fn default_placement(kind: LayerGeomKind) -> Placement {
    match kind {
        LayerGeomKind::Line => Placement::Line {
            repeat_m: 250.0,
            max_angle_delta_deg: 25.0,
        },
        LayerGeomKind::Polygon => Placement::Polygon {
            strategy: PolygonStrategy::Polylabel,
        },
        LayerGeomKind::Point => Placement::Point,
    }
}

/// Compiled stylesheet, keyed by style name. Both maps share style structs
/// behind `Arc` so the runtime can clone references without re-allocating
/// per-feature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stylesheet {
    #[serde(default)]
    pub geometry: std::collections::BTreeMap<String, Arc<Style>>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, Arc<LabelStyle>>,
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
        // mirrors: a typical filled polygon style. bare-hex fill survives the
        // FillPaint migration.
        let yaml = "fill: '#fafafa'\nstroke: '#b4b4b4'\nstroke_width: 0.6\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xfa, 0xfa, 0xfa, 0xff)));
        assert_eq!(s.stroke.unwrap(), Colour::rgba(0xb4, 0xb4, 0xb4, 0xff));
        assert!((s.stroke_width.unwrap() - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn fill_paint_solid_yaml_roundtrip_bare_hex() {
        let yaml = "'#fafafa'\n";
        let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(fp, FillPaint::Solid(c) if c == Colour::rgba(0xfa, 0xfa, 0xfa, 0xff)));
        let out = serde_yaml_ng::to_string(&fp).unwrap();
        assert_eq!(out.trim(), "'#fafafa'");
    }

    #[test]
    fn fill_paint_solid_tagged_form_also_parses() {
        let yaml = "kind: solid\ncolour: '#010203'\n";
        let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(fp, FillPaint::Solid(c) if c == Colour::rgba(1, 2, 3, 0xff)));
    }

    #[test]
    fn fill_paint_hatch_yaml_roundtrip_tagged() {
        let yaml = "kind: hatch\nspacing: 4.0\nangle_deg: 45.0\nline_width: 0.5\ncolour: '#404040'\n";
        let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
        match fp {
            FillPaint::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            } => {
                assert!((spacing - 4.0).abs() < f32::EPSILON);
                assert!((angle_deg - 45.0).abs() < f32::EPSILON);
                assert!((line_width - 0.5).abs() < f32::EPSILON);
                assert_eq!(colour, Colour::rgba(0x40, 0x40, 0x40, 0xff));
            }
            _ => panic!("expected hatch"),
        }
        let out = serde_yaml_ng::to_string(&fp).unwrap();
        assert!(out.contains("kind: hatch"));
        assert!(out.contains("spacing: 4.0"));
        assert!(out.contains("angle_deg: 45.0"));
        assert!(out.contains("line_width: 0.5"));
        assert!(out.contains("colour: '#404040'"));
    }

    #[test]
    fn marker_symbol_yaml_roundtrip() {
        let yaml = "kind: circle\nsize: 8.0\n";
        let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(m, MarkerSymbol::Circle { size } if (size - 8.0).abs() < f32::EPSILON));
        let out = serde_yaml_ng::to_string(&m).unwrap();
        assert!(out.contains("kind: circle"));
        assert!(out.contains("size: 8"));
    }

    #[test]
    fn marker_symbol_default_size_kicks_in() {
        let m: MarkerSymbol = serde_yaml_ng::from_str("kind: triangle").unwrap();
        assert!(matches!(m, MarkerSymbol::Triangle { size } if (size - 6.0).abs() < f32::EPSILON));
    }

    #[test]
    fn style_with_marker_roundtrip() {
        let yaml = "stroke: '#000000'\nstroke_width: 1.0\nfill: '#ff0000'\nmarker:\n  kind: pin\n  size: 10.0\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xff, 0, 0, 0xff)));
        assert!(matches!(s.marker, Some(MarkerSymbol::Pin { size }) if (size - 10.0).abs() < f32::EPSILON));
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
                strategy: PolygonStrategy::Polylabel
            }
        ));

        let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: polylabel").unwrap();
        assert!(matches!(
            p,
            Placement::Polygon {
                strategy: PolygonStrategy::Polylabel
            }
        ));

        let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: centroid").unwrap();
        assert!(matches!(
            p,
            Placement::Polygon {
                strategy: PolygonStrategy::Centroid
            }
        ));

        // one-release migration alias: legacy `inner_skeleton` must parse and
        // map to Polylabel.
        let p: Placement = serde_yaml_ng::from_str("kind: polygon\nstrategy: inner_skeleton").unwrap();
        assert!(matches!(
            p,
            Placement::Polygon {
                strategy: PolygonStrategy::Polylabel
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
                strategy: PolygonStrategy::Polylabel
            }
        ));
    }

    #[test]
    fn label_survival_round_trips_and_defaults_independent() {
        // default
        assert!(matches!(LabelSurvival::default(), LabelSurvival::Independent));
        // wire form is snake_case
        let i: LabelSurvival = serde_yaml_ng::from_str("independent").unwrap();
        assert!(matches!(i, LabelSurvival::Independent));
        let f: LabelSurvival = serde_yaml_ng::from_str("follow_geometry").unwrap();
        assert!(matches!(f, LabelSurvival::FollowGeometry));
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
