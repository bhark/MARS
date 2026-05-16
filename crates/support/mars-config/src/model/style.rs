use mars_style::{LabelStyle, Style};
use serde::{Deserialize, Serialize};

/// Style entry as seen on the YAML wire. The `type:` field discriminates
/// (`line | polygon | point | label | passes`); geometry kinds all share
/// the same flat single-pass shape, `passes` carries an ordered multi-pass
/// stack, and `label` has its own field set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StyleEntry {
    /// `type: label` - label glyph style.
    Label(LabelStyle),
    /// `type: line` - stroked line style (single pass).
    Line(Style),
    /// `type: polygon` - filled+stroked polygon style (single pass).
    Polygon(Style),
    /// `type: point` - point/marker style (single pass).
    Point(Style),
    /// `type: passes` - ordered multi-pass geometry stack. Empty list is
    /// rejected at config-load.
    Passes {
        /// Ordered list of style passes emitted per feature.
        passes: Vec<Style>,
    },
}

impl StyleEntry {
    /// Borrow the geometry-style passes for this entry. Single-pass variants
    /// return a one-element slice via `std::slice::from_ref`; the multi-pass
    /// variant returns its slice directly. Label entries return `None`.
    #[must_use]
    pub fn as_geometry_passes(&self) -> Option<&[Style]> {
        match self {
            Self::Line(s) | Self::Polygon(s) | Self::Point(s) => Some(std::slice::from_ref(s)),
            Self::Passes { passes } => Some(passes.as_slice()),
            Self::Label(_) => None,
        }
    }

    /// Borrow the inner label style for the `label` variant.
    #[must_use]
    pub fn as_label(&self) -> Option<&LabelStyle> {
        match self {
            Self::Label(l) => Some(l),
            _ => None,
        }
    }
}
