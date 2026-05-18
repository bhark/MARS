//! marker symbol + shape vocabulary.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::numeric::NumericField;
use crate::scaled::ScaledSize;

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
    /// Authored rotation in degrees, counter-clockwise. `None` defers to
    /// the renderer's default orientation. `Some(Attribute)` sources the
    /// rotation from a per-feature column at render time.
    pub angle: Option<NumericField>,
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
        // flat tagged shape: kind + size + shape-specific fields + optional
        // angle.
        let angle_extra = usize::from(self.angle.is_some());
        match &self.shape {
            MarkerShape::Circle
            | MarkerShape::Square
            | MarkerShape::Triangle
            | MarkerShape::Cross
            | MarkerShape::X
            | MarkerShape::Pin => {
                let kind = marker_shape_simple_tag(&self.shape);
                let mut m = s.serialize_map(Some(2 + angle_extra))?;
                m.serialize_entry("kind", kind)?;
                m.serialize_entry("size", &self.size)?;
                if let Some(a) = &self.angle {
                    m.serialize_entry("angle", a)?;
                }
                m.end()
            }
            MarkerShape::VectorShape { points, anchor, filled } => {
                let mut m = s.serialize_map(Some(5 + angle_extra))?;
                m.serialize_entry("kind", "vector_shape")?;
                m.serialize_entry("points", points)?;
                m.serialize_entry("anchor", anchor)?;
                m.serialize_entry("filled", filled)?;
                m.serialize_entry("size", &self.size)?;
                if let Some(a) = &self.angle {
                    m.serialize_entry("angle", a)?;
                }
                m.end()
            }
            MarkerShape::Glyph { font_family, ch } => {
                let mut m = s.serialize_map(Some(4 + angle_extra))?;
                m.serialize_entry("kind", "glyph")?;
                m.serialize_entry("font_family", font_family)?;
                m.serialize_entry("ch", ch)?;
                m.serialize_entry("size", &self.size)?;
                if let Some(a) = &self.angle {
                    m.serialize_entry("angle", a)?;
                }
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
            #[serde(default)]
            angle: Option<NumericField>,
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
            angle: f.angle,
        })
    }
}

#[cfg(test)]
mod tests;
