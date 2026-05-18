//! Pixel scalar with optional min/max clamp and reference-denom linear scaling.
//!
//! Authored size resolves to a concrete `f32` given the current scale denominator.
//! `ref_denom=None` means "no scaling, only clamp"; `denom == 0` falls back to
//! the same path (no scaling) so callers don't need to special-case the
//! degenerate page-init state.
//!
//! An optional `attribute` column lifts the base value from a feature
//! attribute (mapserver `SIZE [col]`). Per-feature resolution flows attrs in
//! via [`mars_expr::AttributeAccess`]; when the column is absent or
//! non-numeric the field falls back to `base_px` so static authoring keeps
//! working when no row is in scope.

use mars_expr::{AttributeAccess, Literal};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Pixel size that linearly attenuates with the current scale denominator,
/// optionally clamped to `[min_px, max_px]`. Resolves to a concrete `f32` at
/// render time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScaledSize {
    pub base_px: f32,
    pub min_px: Option<f32>,
    pub max_px: Option<f32>,
    pub ref_denom: Option<u64>,
    /// Attribute column to read the base size from. None for purely static
    /// fields; Some when the authored form is `[col]` or
    /// `{ kind: attribute, column: ... }`. Scale attenuation and clamping
    /// still apply after the attribute fetch.
    pub attribute: Option<String>,
}

impl ScaledSize {
    #[must_use]
    pub const fn from_px(base_px: f32) -> Self {
        Self {
            base_px,
            min_px: None,
            max_px: None,
            ref_denom: None,
            attribute: None,
        }
    }

    /// True when this field references a feature attribute. Drives the
    /// `Style::needs_attributes` walk that decides whether to open the
    /// artifact's attribute section.
    #[must_use]
    pub fn needs_attributes(&self) -> bool {
        self.attribute.is_some()
    }

    /// Concrete pixel size at the current denom against a null attribute
    /// row. Attribute-sourced sizes fall back to `base_px` when no row is
    /// in scope. Convenience for legends, tests and other seams that
    /// resolve outside the per-feature loop.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> f32 {
        self.resolve_with_attrs(denom, &mars_expr::NullAttributes)
    }

    /// Concrete pixel size at the current denom. `ref_denom=None` (or
    /// `denom == 0`) skips scaling and only applies the clamp. When the
    /// authored form references an attribute and the column resolves to a
    /// numeric, the fetched value replaces `base_px`; otherwise `base_px`
    /// is used as the fallback so static authoring stays correct under
    /// attribute-less callers.
    #[must_use]
    pub fn resolve_with_attrs(&self, denom: u64, attrs: &dyn AttributeAccess) -> f32 {
        let base = match &self.attribute {
            Some(name) => attr_as_f32(attrs, name).unwrap_or(self.base_px),
            None => self.base_px,
        };
        let scaled = match self.ref_denom {
            Some(ref_d) if denom != 0 => base * (ref_d as f32 / denom as f32),
            _ => base,
        };
        let lo = self.min_px.unwrap_or(f32::NEG_INFINITY);
        let hi = self.max_px.unwrap_or(f32::INFINITY);
        scaled.clamp(lo, hi)
    }

    fn is_bare(&self) -> bool {
        self.min_px.is_none() && self.max_px.is_none() && self.ref_denom.is_none() && self.attribute.is_none()
    }
}

fn attr_as_f32(attrs: &dyn AttributeAccess, name: &str) -> Option<f32> {
    match attrs.get(name)? {
        Literal::Int(n) => Some(n as f32),
        Literal::Float(v) => Some(v as f32),
        Literal::String(s) => s.parse::<f32>().ok(),
        Literal::Null | Literal::Bool(_) => None,
    }
}

impl From<f32> for ScaledSize {
    fn from(v: f32) -> Self {
        Self::from_px(v)
    }
}

impl Serialize for ScaledSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        if self.is_bare() {
            // bare f32 keeps wire-format symmetry with the original
            // `Option<f32>` shape so existing configs and goldens stay clean.
            return s.serialize_f32(self.base_px);
        }
        let mut len = 1;
        if self.min_px.is_some() {
            len += 1;
        }
        if self.max_px.is_some() {
            len += 1;
        }
        if self.ref_denom.is_some() {
            len += 1;
        }
        if self.attribute.is_some() {
            len += 1;
        }
        let mut st = s.serialize_struct("ScaledSize", len)?;
        st.serialize_field("base_px", &self.base_px)?;
        if let Some(v) = self.min_px {
            st.serialize_field("min_px", &v)?;
        }
        if let Some(v) = self.max_px {
            st.serialize_field("max_px", &v)?;
        }
        if let Some(v) = self.ref_denom {
            st.serialize_field("ref_denom", &v)?;
        }
        if let Some(ref v) = self.attribute {
            st.serialize_field("attribute", v)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for ScaledSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ScaledSizeVisitor)
    }
}

struct ScaledSizeVisitor;

impl<'de> serde::de::Visitor<'de> for ScaledSizeVisitor {
    type Value = ScaledSize;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number, a \"[col]\" string, or a map { base_px, min_px?, max_px?, ref_denom?, attribute? }")
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(ScaledSize::from_px(v as f32))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(ScaledSize::from_px(v as f32))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(ScaledSize::from_px(v as f32))
    }
    fn visit_f32<E: serde::de::Error>(self, v: f32) -> Result<Self::Value, E> {
        Ok(ScaledSize::from_px(v))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let inner = v
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| E::custom(format!("expected `[ident]`, got `{v}`")))?;
        if !is_ident(inner) {
            return Err(E::custom(format!("invalid attribute identifier `{inner}`")));
        }
        Ok(ScaledSize {
            base_px: 0.0,
            min_px: None,
            max_px: None,
            ref_denom: None,
            attribute: Some(inner.to_owned()),
        })
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.visit_str(&v)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        #[derive(Deserialize)]
        struct Tagged {
            #[serde(default)]
            base_px: f32,
            #[serde(default)]
            min_px: Option<f32>,
            #[serde(default)]
            max_px: Option<f32>,
            #[serde(default)]
            ref_denom: Option<u64>,
            #[serde(default)]
            attribute: Option<String>,
        }
        let t = Tagged::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        if let Some(ref col) = t.attribute
            && !is_ident(col)
        {
            return Err(serde::de::Error::custom(format!(
                "invalid attribute identifier `{col}`"
            )));
        }
        Ok(ScaledSize {
            base_px: t.base_px,
            min_px: t.min_px,
            max_px: t.max_px,
            ref_denom: t.ref_denom,
            attribute: t.attribute,
        })
    }
}

// matches mars_expr's interpolate ident rules: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_ident(s: &str) -> bool {
    let mut it = s.bytes();
    let Some(first) = it.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    it.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests;
