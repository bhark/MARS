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
mod tests;
