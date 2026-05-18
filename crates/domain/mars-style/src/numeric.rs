//! Numeric style field: a static literal or an attribute reference.
//!
//! Attribute references resolve per-feature via [`mars_expr::AttributeAccess`].
//! The wire form accepts the importer's `"[col]"` sugar so mapfile imports
//! round-trip terse strings rather than verbose tagged maps.

use mars_expr::{AttributeAccess, Literal};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Numeric source: a static literal, or a column reference resolved at
/// render time.
#[derive(Debug, Clone, PartialEq)]
pub enum NumericField {
    Static(f32),
    Attribute(String),
}

impl NumericField {
    /// Resolve to a concrete `f32` using the feature's attribute row.
    /// Returns `None` when the attribute is missing, NULL, or can't be
    /// coerced to a number. Caller decides drop semantics.
    #[must_use]
    pub fn resolve(&self, attrs: &dyn AttributeAccess) -> Option<f32> {
        match self {
            Self::Static(v) => Some(*v),
            Self::Attribute(name) => match attrs.get(name)? {
                Literal::Int(n) => Some(n as f32),
                Literal::Float(v) => Some(v as f32),
                Literal::String(s) => s.parse::<f32>().ok(),
                Literal::Null | Literal::Bool(_) => None,
            },
        }
    }
}

impl Serialize for NumericField {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Static(v) => s.serialize_f32(*v),
            Self::Attribute(name) => s.collect_str(&format_args!("[{name}]")),
        }
    }
}

impl<'de> Deserialize<'de> for NumericField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(NumericFieldVisitor)
    }
}

struct NumericFieldVisitor;

impl<'de> serde::de::Visitor<'de> for NumericFieldVisitor {
    type Value = NumericField;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number, a \"[col]\" string, or a tagged map { kind: static|attribute, ... }")
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(NumericField::Static(v as f32))
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(NumericField::Static(v as f32))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Ok(NumericField::Static(v as f32))
    }
    fn visit_f32<E: serde::de::Error>(self, v: f32) -> Result<Self::Value, E> {
        Ok(NumericField::Static(v))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let inner = v
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| E::custom(format!("expected `[ident]`, got `{v}`")))?;
        if !is_ident(inner) {
            return Err(E::custom(format!("invalid attribute identifier `{inner}`")));
        }
        Ok(NumericField::Attribute(inner.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.visit_str(&v)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Tagged {
            Static { value: f32 },
            Attribute { column: String },
        }
        let t = Tagged::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        match t {
            Tagged::Static { value } => Ok(NumericField::Static(value)),
            Tagged::Attribute { column } => {
                if !is_ident(&column) {
                    return Err(serde::de::Error::custom(format!(
                        "invalid attribute identifier `{column}`"
                    )));
                }
                Ok(NumericField::Attribute(column))
            }
        }
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
