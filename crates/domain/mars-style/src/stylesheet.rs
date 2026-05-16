//! compiled stylesheet container.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::label::LabelStyle;
use crate::style::Style;

/// Compiled stylesheet, keyed by style name. Geometry entries carry an
/// ordered list of style passes (`Arc<[Style]>`); single-pass entries store a
/// one-element slice. The runtime renders each pass in declared order, so a
/// class can stack fill + stroke + marker passes without per-feature
/// composition logic on the hot path. Label entries remain single-style.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stylesheet {
    #[serde(default)]
    pub geometry: BTreeMap<String, Arc<[Style]>>,
    #[serde(default)]
    pub labels: BTreeMap<String, Arc<LabelStyle>>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::colour::Colour;
    use crate::scaled::ScaledSize;

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
}
