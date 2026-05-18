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
mod tests;
