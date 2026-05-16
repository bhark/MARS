//! MARS style model. a small fixed vocabulary close to SVG.
//!
//! No rendering happens here; the renderer adapter consumes the compiled form.

#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

mod numeric;
mod scaled;

pub use numeric::NumericField;
pub use scaled::ScaledSize;

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

/// Point marker symbol: a shape (`MarkerShape`) plus a pixel size. The wire
/// form is a flat tagged map (`kind: <shape>`, `size: <f32>`, plus shape-
/// specific fields) so existing configs and goldens stay diff-clean across
/// the shape extraction. Glyph defaults `size` to 12; everything else to 6.
/// `size` is a `ScaledSize` so authored markers can attenuate with the
/// scale denom; bare-`f32` wire forms remain accepted via `ScaledSize`'s
/// serde.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkerSymbol {
    pub shape: MarkerShape,
    pub size: ScaledSize,
}

/// Point marker shape. Shape-specific bodies stay inside the variant; common
/// knobs (`size`) live on the enclosing `MarkerSymbol`. Dispatch is
/// exhaustive on purpose so a new variant breaks the build at every match
/// site (see `docs/EXTENDING.md` principle 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarkerShape {
    Circle,
    Square,
    Triangle,
    Cross,
    X,
    Pin,
    /// Arbitrary closed polygon described by a point list in a unit-square
    /// local frame; `anchor` is the local-frame position that maps to the
    /// feature anchor. mirrors mapserver SYMBOL TYPE VECTOR with explicit
    /// POINTS.
    VectorShape {
        /// Polygon vertices in the symbol's local frame. The frame is
        /// normalised to `[0, 1] x [0, 1]` by mapserver convention; the
        /// renderer scales by `MarkerSymbol::size`.
        points: Vec<(f32, f32)>,
        /// Local-frame point that maps to the feature anchor. Defaults to
        /// the local-frame centre `(0.5, 0.5)`.
        #[serde(default = "MarkerShape::default_vector_anchor")]
        anchor: (f32, f32),
        /// True for a filled polygon, false for an open polyline stroke.
        #[serde(default = "MarkerShape::default_filled")]
        filled: bool,
    },
    /// Single text glyph rasterised from a registered font. Used for
    /// mapfile `SYMBOL TYPE TRUETYPE` with a `CHARACTER` body.
    Glyph {
        font_family: String,
        /// Glyph character (or grapheme cluster). The renderer shapes and
        /// rasterises it the same way a label run is shaped.
        #[serde(alias = "character")]
        ch: String,
    },
}

impl MarkerShape {
    const fn default_vector_anchor() -> (f32, f32) {
        (0.5, 0.5)
    }

    const fn default_filled() -> bool {
        true
    }
}

impl MarkerSymbol {
    /// Per-shape default size in pixels: Glyph defaults to 12; every other
    /// shape defaults to 6. Mirrors the pre-extraction enum-level defaults.
    #[must_use]
    pub const fn default_size_for(shape_is_glyph: bool) -> f32 {
        if shape_is_glyph { 12.0 } else { 6.0 }
    }

    /// Authored base size in pixels (pre-resolve). Renderer code consumes
    /// the resolved variant; this accessor is for config validators that
    /// check the authored value before any denom is in scope.
    #[must_use]
    pub fn base_size(&self) -> f32 {
        self.size.base_px
    }
}

impl Serialize for MarkerSymbol {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // flat tagged shape: kind + size + shape-specific fields.
        match &self.shape {
            MarkerShape::Circle
            | MarkerShape::Square
            | MarkerShape::Triangle
            | MarkerShape::Cross
            | MarkerShape::X
            | MarkerShape::Pin => {
                let kind = marker_shape_simple_tag(&self.shape);
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("kind", kind)?;
                m.serialize_entry("size", &self.size)?;
                m.end()
            }
            MarkerShape::VectorShape { points, anchor, filled } => {
                let mut m = s.serialize_map(Some(5))?;
                m.serialize_entry("kind", "vector_shape")?;
                m.serialize_entry("points", points)?;
                m.serialize_entry("anchor", anchor)?;
                m.serialize_entry("filled", filled)?;
                m.serialize_entry("size", &self.size)?;
                m.end()
            }
            MarkerShape::Glyph { font_family, ch } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("kind", "glyph")?;
                m.serialize_entry("font_family", font_family)?;
                m.serialize_entry("ch", ch)?;
                m.serialize_entry("size", &self.size)?;
                m.end()
            }
        }
    }
}

fn marker_shape_simple_tag(shape: &MarkerShape) -> &'static str {
    match shape {
        MarkerShape::Circle => "circle",
        MarkerShape::Square => "square",
        MarkerShape::Triangle => "triangle",
        MarkerShape::Cross => "cross",
        MarkerShape::X => "x",
        MarkerShape::Pin => "pin",
        // these never reach here; the caller dispatches on the variant.
        MarkerShape::VectorShape { .. } => "vector_shape",
        MarkerShape::Glyph { .. } => "glyph",
    }
}

impl<'de> Deserialize<'de> for MarkerSymbol {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // flat tagged map: read `kind:` then dispatch to a per-shape body
        // that picks up `size` plus the shape-specific fields. preserves
        // wire-format compatibility with the pre-extraction enum. `size`
        // routes through `ScaledSize`, so bare f32 / int and the tagged
        // `{ base_px, min_px, max_px, ref_denom }` form both work.
        #[derive(Deserialize)]
        struct Flat {
            kind: String,
            #[serde(default)]
            size: Option<ScaledSize>,
            // vector_shape body
            #[serde(default)]
            points: Option<Vec<(f32, f32)>>,
            #[serde(default)]
            anchor: Option<(f32, f32)>,
            #[serde(default)]
            filled: Option<bool>,
            // glyph body
            #[serde(default)]
            font_family: Option<String>,
            #[serde(default, alias = "character")]
            ch: Option<String>,
        }
        let f = Flat::deserialize(d)?;
        let (shape, default_size) = match f.kind.as_str() {
            "circle" => (MarkerShape::Circle, 6.0),
            "square" => (MarkerShape::Square, 6.0),
            "triangle" => (MarkerShape::Triangle, 6.0),
            "cross" => (MarkerShape::Cross, 6.0),
            "x" => (MarkerShape::X, 6.0),
            "pin" => (MarkerShape::Pin, 6.0),
            "vector_shape" => {
                let points = f.points.ok_or_else(|| serde::de::Error::missing_field("points"))?;
                let anchor = f.anchor.unwrap_or_else(MarkerShape::default_vector_anchor);
                let filled = f.filled.unwrap_or_else(MarkerShape::default_filled);
                (MarkerShape::VectorShape { points, anchor, filled }, 6.0)
            }
            "glyph" => {
                let font_family = f
                    .font_family
                    .ok_or_else(|| serde::de::Error::missing_field("font_family"))?;
                let ch = f.ch.ok_or_else(|| serde::de::Error::missing_field("ch"))?;
                (MarkerShape::Glyph { font_family, ch }, 12.0)
            }
            other => {
                return Err(serde::de::Error::unknown_variant(
                    other,
                    &[
                        "circle",
                        "square",
                        "triangle",
                        "cross",
                        "x",
                        "pin",
                        "vector_shape",
                        "glyph",
                    ],
                ));
            }
        };
        Ok(MarkerSymbol {
            shape,
            size: f.size.unwrap_or_else(|| ScaledSize::from_px(default_size)),
        })
    }
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

/// Polygon fill paint. `Solid` is a bare hex string on the wire; `Hatch`
/// and `Image` are tagged maps. Dispatch is exhaustive on purpose so a new
/// variant breaks the build at every match site (see `docs/EXTENDING.md`
/// principle 2).
///
/// Procedural variants (`Solid`, `Hatch`) reach the renderer through
/// `DrawOp::Path` and the `fill/` dispatcher. Non-procedural variants
/// (`Image`, future `Svg`) reach the renderer through `DrawOp::Pattern`
/// and the `pattern/` dispatcher; the runtime is responsible for picking
/// the right DrawOp variant per fill paint.
#[derive(Debug, Clone, PartialEq)]
pub enum FillPaint {
    Solid(Colour),
    Hatch {
        spacing: f32,
        angle_deg: f32,
        line_width: f32,
        colour: Colour,
    },
    /// Tiled non-procedural image pattern. `name` keys into a renderer-
    /// side image registry (analog of the `Fonts` registry passed to the
    /// rasteriser); opacity flows from `Style::opacity`. Sizing /
    /// rotation knobs are deferred until a concrete need surfaces.
    Image {
        name: String,
    },
}

impl Serialize for FillPaint {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
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
                st.serialize_field("spacing", spacing)?;
                st.serialize_field("angle_deg", angle_deg)?;
                st.serialize_field("line_width", line_width)?;
                st.serialize_field("colour", colour)?;
                st.end()
            }
            Self::Image { name } => {
                let mut st = s.serialize_struct("Image", 2)?;
                st.serialize_field("kind", "image")?;
                st.serialize_field("name", name)?;
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

struct FillPaintVisitor;

impl<'de> serde::de::Visitor<'de> for FillPaintVisitor {
    type Value = FillPaint;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a hex colour string (#rrggbb / #rrggbbaa) or a tagged map (kind: solid|hatch|image)")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Colour::from_str(v).map(FillPaint::Solid).map_err(E::custom)
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.visit_str(&v)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        // tagged shim: serde derives Deserialize over the field-set, we map
        // its variants onto FillPaint inline so no module-level helper type
        // leaks.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Tagged {
            Solid {
                colour: Colour,
            },
            Hatch {
                spacing: f32,
                angle_deg: f32,
                line_width: f32,
                colour: Colour,
            },
            Image {
                name: String,
            },
        }
        let tagged = Tagged::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(match tagged {
            Tagged::Solid { colour } => FillPaint::Solid(colour),
            Tagged::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            } => FillPaint::Hatch {
                spacing,
                angle_deg,
                line_width,
                colour,
            },
            Tagged::Image { name } => FillPaint::Image { name },
        })
    }
}

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
}

impl Style {
    /// Resolve every size-like authored field against `denom` and return a
    /// renderer-facing variant with concrete pixel scalars. The renderer
    /// never sees `ScaledSize` directly; this is the seam.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> ResolvedStyle {
        ResolvedStyle {
            fill: self.fill.clone(),
            stroke: self.stroke,
            stroke_width: self.stroke_width.map(|s| s.resolve(denom)),
            stroke_dasharray: self.stroke_dasharray.clone(),
            stroke_linecap: self.stroke_linecap,
            stroke_linejoin: self.stroke_linejoin,
            marker: self.marker.as_ref().map(|m| ResolvedMarker {
                shape: m.shape.clone(),
                size: m.size.resolve(denom),
            }),
            opacity: self.opacity,
            stroke_offset_px: self.stroke_offset_px,
            stroke_gap: self.stroke_gap,
            geom_transform: self.geom_transform,
        }
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
}

/// Resolved marker: shape unchanged from authored form, `size` collapsed
/// to a concrete pixel value.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedMarker {
    pub shape: MarkerShape,
    pub size: f32,
}

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
    /// Static label rotation in degrees, counter-clockwise. `None` defers
    /// to the placement-derived angle (zero for points/polygons, tangent
    /// for lines). Mirrors mapserver's numeric `ANGLE <deg>`.
    #[serde(default)]
    pub angle_deg: Option<f32>,
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
    /// renderer-facing variant.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> ResolvedLabelStyle {
        ResolvedLabelStyle {
            font_family: self.font_family.clone(),
            font_size: self.font_size.resolve(denom),
            fill: self.fill,
            halo: self.halo.clone(),
            priority: self.priority,
            min_distance: self.min_distance,
            position: self.position,
            offset_px: self.offset_px,
            angle_deg: self.angle_deg,
            partials: self.partials,
            force: self.force,
        }
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
    const fn default_repeat_m() -> f64 {
        250.0
    }
    const fn default_max_angle_delta_deg() -> f32 {
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

/// Layer geometry kind. Mirrors the layer `type:` field in service config
/// for vector layers. Raster layers are discriminated one level up via
/// [`LayerKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerGeomKind {
    Point,
    Line,
    Polygon,
}

impl LayerGeomKind {
    /// Parse the `type:` field of a vector layer. Returns `None` for raster
    /// or unknown values; use [`LayerKind::parse`] when the caller needs to
    /// distinguish vector vs raster.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "point" => Some(Self::Point),
            "line" => Some(Self::Line),
            "polygon" => Some(Self::Polygon),
            _ => None,
        }
    }
}

/// Top-level layer kind: vector (with an inner geometry kind) or raster.
/// Dispatch sites that branch the compiler / runtime pipeline match on this
/// enum; adding a variant breaks compilation at every dispatch hub by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// Vector layer with a specific geometry kind.
    Vector(LayerGeomKind),
    /// Raster layer. Source binding and render path are not vector-shaped.
    Raster,
}

impl LayerKind {
    /// Parse the `type:` field of a layer config. Accepts the vector kinds
    /// understood by [`LayerGeomKind`] plus `"raster"`. Returns `None` for
    /// unknown values; callers decide whether to fall back or reject.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(g) = LayerGeomKind::parse(s) {
            return Some(Self::Vector(g));
        }
        match s {
            "raster" => Some(Self::Raster),
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
            angle_mode: LineAngleMode::Auto,
        },
        LayerGeomKind::Polygon => Placement::Polygon {
            strategy: PolygonStrategy::Polylabel,
        },
        LayerGeomKind::Point => Placement::Point,
    }
}

/// Compiled stylesheet, keyed by style name. Geometry entries carry an
/// ordered list of style passes (`Arc<[Style]>`); single-pass entries store a
/// one-element slice. The runtime renders each pass in declared order, so a
/// class can stack fill + stroke + marker passes without per-feature
/// composition logic on the hot path. Label entries remain single-style.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stylesheet {
    #[serde(default)]
    pub geometry: std::collections::BTreeMap<String, Arc<[Style]>>,
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
        // bare-hex fill must deserialise as Solid (wire-format symmetry).
        let yaml = "fill: '#fafafa'\nstroke: '#b4b4b4'\nstroke_width: 0.6\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xfa, 0xfa, 0xfa, 0xff)));
        assert_eq!(s.stroke.unwrap(), Colour::rgba(0xb4, 0xb4, 0xb4, 0xff));
        // bare f32 wire form lands in ScaledSize::from_px.
        assert_eq!(s.stroke_width.unwrap(), ScaledSize::from_px(0.6));
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
    fn fill_paint_image_yaml_roundtrip_tagged() {
        let yaml = "kind: image\nname: brick\n";
        let fp: FillPaint = serde_yaml_ng::from_str(yaml).unwrap();
        match &fp {
            FillPaint::Image { name } => assert_eq!(name, "brick"),
            _ => panic!("expected image"),
        }
        let out = serde_yaml_ng::to_string(&fp).unwrap();
        assert!(out.contains("kind: image"));
        assert!(out.contains("name: brick"));
    }

    #[test]
    fn marker_symbol_yaml_roundtrip() {
        let yaml = "kind: circle\nsize: 8.0\n";
        let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(m.shape, MarkerShape::Circle);
        assert!((m.size.base_px - 8.0).abs() < f32::EPSILON);
        let out = serde_yaml_ng::to_string(&m).unwrap();
        assert!(out.contains("kind: circle"));
        assert!(out.contains("size: 8"));
    }

    #[test]
    fn marker_symbol_default_size_kicks_in() {
        let m: MarkerSymbol = serde_yaml_ng::from_str("kind: triangle").unwrap();
        assert_eq!(m.shape, MarkerShape::Triangle);
        assert!((m.size.base_px - 6.0).abs() < f32::EPSILON);
    }

    #[test]
    fn marker_symbol_default_size_for_glyph_is_twelve() {
        let yaml = "kind: glyph\nfont_family: Sans\nch: A\n";
        let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(m.shape, MarkerShape::Glyph { .. }));
        assert!((m.size.base_px - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn style_with_marker_roundtrip() {
        let yaml = "stroke: '#000000'\nstroke_width: 1.0\nfill: '#ff0000'\nmarker:\n  kind: pin\n  size: 10.0\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(s.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xff, 0, 0, 0xff)));
        let m = s.marker.expect("marker present");
        assert_eq!(m.shape, MarkerShape::Pin);
        assert!((m.size.base_px - 10.0).abs() < f32::EPSILON);
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
                angle_mode,
            } => {
                assert!((repeat_m - 250.0).abs() < f64::EPSILON);
                assert!((max_angle_delta_deg - 25.0).abs() < f32::EPSILON);
                assert_eq!(angle_mode, LineAngleMode::Auto);
            }
            _ => panic!("expected line"),
        }

        let p: Placement = serde_yaml_ng::from_str("kind: line\nrepeat_m: 100\nmax_angle_delta_deg: 10").unwrap();
        match p {
            Placement::Line {
                repeat_m,
                max_angle_delta_deg,
                angle_mode,
            } => {
                assert!((repeat_m - 100.0).abs() < f64::EPSILON);
                assert!((max_angle_delta_deg - 10.0).abs() < f32::EPSILON);
                assert_eq!(angle_mode, LineAngleMode::Auto);
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
    fn layer_kind_parses_vector_and_raster() {
        assert!(matches!(
            LayerKind::parse("point"),
            Some(LayerKind::Vector(LayerGeomKind::Point))
        ));
        assert!(matches!(
            LayerKind::parse("line"),
            Some(LayerKind::Vector(LayerGeomKind::Line))
        ));
        assert!(matches!(
            LayerKind::parse("polygon"),
            Some(LayerKind::Vector(LayerGeomKind::Polygon))
        ));
        assert!(matches!(LayerKind::parse("raster"), Some(LayerKind::Raster)));
        assert_eq!(LayerKind::parse("query"), None);
        assert_eq!(LayerKind::parse(""), None);
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
    fn style_opacity_offset_gap_default_to_none() {
        let s = Style::default();
        assert!(s.opacity.is_none());
        assert!(s.stroke_offset_px.is_none());
        assert!(s.stroke_gap.is_none());
    }

    #[test]
    fn style_opacity_offset_gap_roundtrip_yaml() {
        let yaml = "stroke: '#000000'\nstroke_width: 1.0\nopacity: 0.5\nstroke_offset_px: 2.0\nstroke_gap:\n  interval_px: 12.0\n  initial_px: 3.0\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert!((s.opacity.unwrap() - 0.5).abs() < f32::EPSILON);
        assert!((s.stroke_offset_px.unwrap() - 2.0).abs() < f32::EPSILON);
        let g = s.stroke_gap.unwrap();
        assert!((g.interval_px - 12.0).abs() < f32::EPSILON);
        assert!((g.initial_px - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stroke_gap_initial_defaults_to_zero() {
        let g: StrokeGap = serde_yaml_ng::from_str("interval_px: 8.0\n").unwrap();
        assert!((g.interval_px - 8.0).abs() < f32::EPSILON);
        assert!(g.initial_px.abs() < f32::EPSILON);
    }

    #[test]
    fn marker_vector_shape_roundtrip() {
        let yaml = "kind: vector_shape\npoints: [[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]]\nsize: 10.0\n";
        let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
        match m.shape {
            MarkerShape::VectorShape { points, anchor, filled } => {
                assert_eq!(points.len(), 3);
                assert!((anchor.0 - 0.5).abs() < f32::EPSILON);
                assert!((anchor.1 - 0.5).abs() < f32::EPSILON);
                assert!(filled);
            }
            _ => panic!("expected vector_shape"),
        }
        assert!((m.size.base_px - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn marker_glyph_roundtrip_accepts_character_alias() {
        let yaml = "kind: glyph\nfont_family: \"Sans\"\ncharacter: \"T\"\nsize: 14.0\n";
        let m: MarkerSymbol = serde_yaml_ng::from_str(yaml).unwrap();
        match m.shape {
            MarkerShape::Glyph { font_family, ch } => {
                assert_eq!(font_family, "Sans");
                assert_eq!(ch, "T");
            }
            _ => panic!("expected glyph"),
        }
        assert!((m.size.base_px - 14.0).abs() < f32::EPSILON);
    }

    #[test]
    fn marker_base_size_works_for_all_variants() {
        assert!(
            (MarkerSymbol {
                shape: MarkerShape::Circle,
                size: ScaledSize::from_px(7.0),
            }
            .base_size()
                - 7.0)
                .abs()
                < f32::EPSILON
        );
        assert!(
            (MarkerSymbol {
                shape: MarkerShape::VectorShape {
                    points: vec![(0.0, 0.0), (1.0, 0.0), (0.5, 1.0)],
                    anchor: (0.5, 0.5),
                    filled: true,
                },
                size: ScaledSize::from_px(9.0),
            }
            .base_size()
                - 9.0)
                .abs()
                < f32::EPSILON
        );
        assert!(
            (MarkerSymbol {
                shape: MarkerShape::Glyph {
                    font_family: "Sans".into(),
                    ch: "X".into(),
                },
                size: ScaledSize::from_px(11.0),
            }
            .base_size()
                - 11.0)
                .abs()
                < f32::EPSILON
        );
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
        assert_eq!(l.font_size, ScaledSize::from_px(12.0));
        assert_eq!(l.fill, Colour::rgba(0, 0, 0, 0xff));
        let halo = l.halo.unwrap();
        assert_eq!(halo.colour, Colour::rgba(0xff, 0xff, 0xff, 0xff));
        assert!((halo.width - 1.5).abs() < f32::EPSILON);
        // new fields default to the back-compat values so existing configs
        // keep their current behaviour.
        assert_eq!(l.position, AnchorPosition::Auto);
        assert_eq!(l.offset_px, (0.0, 0.0));
        assert!(l.angle_deg.is_none());
        assert!(!l.partials);
        assert!(!l.force);
    }

    #[test]
    fn label_style_round_trips_new_fields() {
        let yaml = r#"
font_family: Arial
font_size: 12
fill: '#000000'
position: uc
offset_px: [3.5, -2.0]
angle_deg: 45.0
partials: true
force: true
min_distance: 8.0
"#;
        let l: LabelStyle = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(l.position, AnchorPosition::Uc);
        assert_eq!(l.offset_px, (3.5, -2.0));
        assert_eq!(l.angle_deg, Some(45.0));
        assert!(l.partials);
        assert!(l.force);
        assert!((l.min_distance - 8.0).abs() < f32::EPSILON);

        // serialise back and reparse: round-trip must preserve the new fields.
        let out = serde_yaml_ng::to_string(&l).unwrap();
        let back: LabelStyle = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back, l);
    }

    #[test]
    fn anchor_position_wire_form_is_short_lowercase() {
        for (pos, wire) in [
            (AnchorPosition::Ul, "ul"),
            (AnchorPosition::Uc, "uc"),
            (AnchorPosition::Ur, "ur"),
            (AnchorPosition::Cl, "cl"),
            (AnchorPosition::Cc, "cc"),
            (AnchorPosition::Cr, "cr"),
            (AnchorPosition::Ll, "ll"),
            (AnchorPosition::Lc, "lc"),
            (AnchorPosition::Lr, "lr"),
            (AnchorPosition::Auto, "auto"),
        ] {
            let out = serde_yaml_ng::to_string(&pos).unwrap();
            assert_eq!(out.trim(), wire);
            let back: AnchorPosition = serde_yaml_ng::from_str(wire).unwrap();
            assert_eq!(back, pos);
        }
    }

    #[test]
    fn line_angle_mode_round_trips() {
        let p: Placement = serde_yaml_ng::from_str("kind: line\nangle_mode: follow").unwrap();
        match p {
            Placement::Line { angle_mode, .. } => assert_eq!(angle_mode, LineAngleMode::Follow),
            _ => panic!("expected line"),
        }
        let p: Placement = serde_yaml_ng::from_str("kind: line\nangle_mode: auto").unwrap();
        match p {
            Placement::Line { angle_mode, .. } => assert_eq!(angle_mode, LineAngleMode::Auto),
            _ => panic!("expected line"),
        }
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

    #[test]
    fn style_geom_transform_defaults_to_none() {
        let s: Style = serde_yaml_ng::from_str("stroke: '#000000'\n").unwrap();
        assert!(s.geom_transform.is_none());
    }

    #[test]
    fn style_with_geom_transform_round_trips() {
        let yaml = "stroke: '#000000'\ngeom_transform: vertices\n";
        let s: Style = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(s.geom_transform, Some(GeomTransform::Vertices));
        let out = serde_yaml_ng::to_string(&s).unwrap();
        assert!(out.contains("geom_transform: vertices"));
    }

    #[test]
    fn stylesheet_geometry_single_pass_round_trips() {
        let mut ss = Stylesheet::default();
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 0xff)),
            stroke_width: Some(ScaledSize::from_px(1.0)),
            ..Default::default()
        };
        ss.geometry.insert("solo".into(), Arc::from(vec![s.clone()]));
        let json = serde_json::to_string(&ss).unwrap();
        let back: Stylesheet = serde_json::from_str(&json).unwrap();
        let passes = back.geometry.get("solo").expect("entry");
        assert_eq!(passes.len(), 1);
        assert_eq!(passes[0], s);
    }

    #[test]
    fn stylesheet_geometry_multi_pass_round_trips() {
        let mut ss = Stylesheet::default();
        let pass_a = Style {
            stroke: Some(Colour::rgba(0xff, 0, 0, 0xff)),
            stroke_width: Some(ScaledSize::from_px(4.0)),
            ..Default::default()
        };
        let pass_b = Style {
            stroke: Some(Colour::rgba(0, 0xff, 0, 0xff)),
            stroke_width: Some(ScaledSize::from_px(1.0)),
            ..Default::default()
        };
        ss.geometry
            .insert("stack".into(), Arc::from(vec![pass_a.clone(), pass_b.clone()]));
        let json = serde_json::to_string(&ss).unwrap();
        let back: Stylesheet = serde_json::from_str(&json).unwrap();
        let passes = back.geometry.get("stack").expect("entry");
        assert_eq!(passes.len(), 2);
        // declared order preserved
        assert_eq!(passes[0], pass_a);
        assert_eq!(passes[1], pass_b);
    }

    #[test]
    fn style_resolve_collapses_stroke_width_against_denom() {
        let s = Style {
            stroke: Some(Colour::rgba(0, 0, 0, 0xff)),
            stroke_width: Some(ScaledSize {
                base_px: 10.0,
                min_px: Some(2.0),
                max_px: Some(20.0),
                ref_denom: Some(50_000),
            }),
            ..Default::default()
        };
        // at half the ref denom: 2x scaling, clamped at max_px=20.
        let r = s.resolve(25_000);
        assert!((r.stroke_width.unwrap() - 20.0).abs() < f32::EPSILON);
        // at 2x: half size, no clamp (5.0).
        let r = s.resolve(100_000);
        assert!((r.stroke_width.unwrap() - 5.0).abs() < f32::EPSILON);
        // far out: clamped to min_px=2.
        let r = s.resolve(2_000_000);
        assert!((r.stroke_width.unwrap() - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn style_resolve_passes_through_non_size_fields_unchanged() {
        let s = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(0xff, 0, 0, 0xff))),
            stroke: Some(Colour::rgba(0, 0xff, 0, 0xff)),
            stroke_width: Some(ScaledSize::from_px(1.5)),
            stroke_dasharray: Some(vec![4.0, 2.0]),
            opacity: Some(0.5),
            ..Default::default()
        };
        let r = s.resolve(50_000);
        assert!(matches!(r.fill, Some(FillPaint::Solid(c)) if c == Colour::rgba(0xff, 0, 0, 0xff)));
        assert_eq!(r.stroke.unwrap(), Colour::rgba(0, 0xff, 0, 0xff));
        assert_eq!(r.stroke_dasharray.as_deref(), Some(&[4.0, 2.0][..]));
        assert!((r.opacity.unwrap() - 0.5).abs() < f32::EPSILON);
        assert!((r.stroke_width.unwrap() - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn style_resolve_marker_size_collapses() {
        let s = Style {
            fill: Some(FillPaint::Solid(Colour::rgba(0, 0, 0, 0xff))),
            marker: Some(MarkerSymbol {
                shape: MarkerShape::Circle,
                size: ScaledSize {
                    base_px: 8.0,
                    min_px: None,
                    max_px: None,
                    ref_denom: Some(50_000),
                },
            }),
            ..Default::default()
        };
        let r = s.resolve(25_000);
        let m = r.marker.expect("marker resolved");
        assert!((m.size - 16.0).abs() < f32::EPSILON);
        assert_eq!(m.shape, MarkerShape::Circle);
    }

    #[test]
    fn label_style_resolve_collapses_font_size() {
        let l = LabelStyle {
            font_family: "Sans".into(),
            font_size: ScaledSize {
                base_px: 12.0,
                min_px: Some(6.0),
                max_px: Some(24.0),
                ref_denom: Some(50_000),
            },
            fill: Colour::rgba(0, 0, 0, 0xff),
            halo: None,
            priority: 100,
            min_distance: 0.0,
            position: AnchorPosition::Auto,
            offset_px: (0.0, 0.0),
            angle_deg: None,
            partials: false,
            force: false,
        };
        let r = l.resolve(25_000);
        assert!((r.font_size - 24.0).abs() < f32::EPSILON);
        assert_eq!(r.font_family, "Sans");
        assert_eq!(r.priority, 100);
    }
}
