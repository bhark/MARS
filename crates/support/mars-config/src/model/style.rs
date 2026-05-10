use mars_style::{LabelStyle, Style};
use serde::{Deserialize, Serialize};

/// Style entry as seen on the YAML wire. The `type:` field discriminates
/// (: `line | polygon | point | label`); geometry kinds all share
/// the same flat shape, label has its own field set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StyleEntry {
    /// `type: label` - label glyph style.
    Label(LabelStyle),
    /// `type: line` - stroked line style.
    Line(Style),
    /// `type: polygon` - filled+stroked polygon style.
    Polygon(Style),
    /// `type: point` - point/marker style.
    Point(Style),
}

impl StyleEntry {
    /// Borrow the inner geometry style for line/polygon/point variants.
    #[must_use]
    pub fn as_geometry(&self) -> Option<&Style> {
        match self {
            Self::Line(s) | Self::Polygon(s) | Self::Point(s) => Some(s),
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
