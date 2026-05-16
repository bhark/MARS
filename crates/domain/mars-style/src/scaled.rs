//! Pixel scalar with optional min/max clamp and reference-denom linear scaling.
//!
//! Authored size resolves to a concrete `f32` given the current scale denominator.
//! `ref_denom=None` means "no scaling, only clamp"; `denom == 0` falls back to
//! the same path (no scaling) so callers don't need to special-case the
//! degenerate page-init state.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Pixel size that linearly attenuates with the current scale denominator,
/// optionally clamped to `[min_px, max_px]`. Resolves to a concrete `f32` at
/// render time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaledSize {
    pub base_px: f32,
    pub min_px: Option<f32>,
    pub max_px: Option<f32>,
    pub ref_denom: Option<u64>,
}

impl ScaledSize {
    #[must_use]
    pub const fn from_px(base_px: f32) -> Self {
        Self {
            base_px,
            min_px: None,
            max_px: None,
            ref_denom: None,
        }
    }

    /// Concrete pixel size at the current denom. `ref_denom=None` (or
    /// `denom == 0`) skips scaling and only applies the clamp.
    #[must_use]
    pub fn resolve(&self, denom: u64) -> f32 {
        let scaled = match self.ref_denom {
            Some(ref_d) if denom != 0 => self.base_px * (ref_d as f32 / denom as f32),
            _ => self.base_px,
        };
        let lo = self.min_px.unwrap_or(f32::NEG_INFINITY);
        let hi = self.max_px.unwrap_or(f32::INFINITY);
        scaled.clamp(lo, hi)
    }

    fn is_bare(&self) -> bool {
        self.min_px.is_none() && self.max_px.is_none() && self.ref_denom.is_none()
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
        f.write_str("a number or a map { base_px, min_px?, max_px?, ref_denom? }")
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

    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        #[derive(Deserialize)]
        struct Tagged {
            base_px: f32,
            #[serde(default)]
            min_px: Option<f32>,
            #[serde(default)]
            max_px: Option<f32>,
            #[serde(default)]
            ref_denom: Option<u64>,
        }
        let t = Tagged::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
        Ok(ScaledSize {
            base_px: t.base_px,
            min_px: t.min_px,
            max_px: t.max_px,
            ref_denom: t.ref_denom,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn from_f32_yields_bare_form() {
        let s: ScaledSize = 8.0_f32.into();
        assert!((s.base_px - 8.0).abs() < f32::EPSILON);
        assert!(s.min_px.is_none() && s.max_px.is_none() && s.ref_denom.is_none());
    }

    #[test]
    fn resolve_without_ref_denom_is_clamp_only() {
        let s = ScaledSize {
            base_px: 10.0,
            min_px: Some(4.0),
            max_px: Some(20.0),
            ref_denom: None,
        };
        assert!((s.resolve(1) - 10.0).abs() < f32::EPSILON);
        assert!((s.resolve(1_000_000) - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_scales_linearly_by_ref_over_denom() {
        let s = ScaledSize {
            base_px: 8.0,
            min_px: None,
            max_px: None,
            ref_denom: Some(50_000),
        };
        // at ref_denom: same size.
        assert!((s.resolve(50_000) - 8.0).abs() < f32::EPSILON);
        // at half the denom: double the size.
        assert!((s.resolve(25_000) - 16.0).abs() < f32::EPSILON);
        // at twice the denom: half the size.
        assert!((s.resolve(100_000) - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_clamps_after_scaling() {
        let s = ScaledSize {
            base_px: 8.0,
            min_px: Some(2.0),
            max_px: Some(16.0),
            ref_denom: Some(50_000),
        };
        // far-out zoom: would be 64, clamped to 16.
        assert!((s.resolve(6_250) - 16.0).abs() < f32::EPSILON);
        // far-in zoom: would be 1, clamped to 2.
        assert!((s.resolve(400_000) - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn resolve_denom_zero_skips_scaling() {
        let s = ScaledSize {
            base_px: 8.0,
            min_px: Some(2.0),
            max_px: Some(16.0),
            ref_denom: Some(50_000),
        };
        assert!((s.resolve(0) - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn bare_f32_roundtrips_via_yaml() {
        let yaml = "8.0\n";
        let s: ScaledSize = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(s, ScaledSize::from_px(8.0));
        let out = serde_yaml_ng::to_string(&s).unwrap();
        assert_eq!(out.trim(), "8.0");
    }

    #[test]
    fn tagged_map_roundtrips_via_yaml() {
        let yaml = "base_px: 8.0\nmin_px: 2.0\nmax_px: 16.0\nref_denom: 50000\n";
        let s: ScaledSize = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(s.base_px, 8.0);
        assert_eq!(s.min_px, Some(2.0));
        assert_eq!(s.max_px, Some(16.0));
        assert_eq!(s.ref_denom, Some(50_000));
        let out = serde_yaml_ng::to_string(&s).unwrap();
        let back: ScaledSize = serde_yaml_ng::from_str(&out).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn tagged_map_drops_unset_fields_on_serialise() {
        let s = ScaledSize {
            base_px: 8.0,
            min_px: Some(2.0),
            max_px: None,
            ref_denom: None,
        };
        let out = serde_yaml_ng::to_string(&s).unwrap();
        assert!(out.contains("base_px: 8"));
        assert!(out.contains("min_px: 2"));
        assert!(!out.contains("max_px"));
        assert!(!out.contains("ref_denom"));
    }

    #[test]
    fn bare_int_yaml_parses() {
        // yaml emits/accepts unquoted integers; ensure that path lands in Static.
        let s: ScaledSize = serde_yaml_ng::from_str("8").unwrap();
        assert_eq!(s, ScaledSize::from_px(8.0));
    }
}
