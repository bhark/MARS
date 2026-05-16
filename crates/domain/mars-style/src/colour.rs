//! colour + fill-paint primitives.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::StyleError;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

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
}
