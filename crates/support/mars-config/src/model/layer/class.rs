use mars_style::Style;
use serde::{Deserialize, Serialize};

use super::{LayerLabel, ScaleWindow};

/// Layer class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Class {
    /// Class identifier.
    pub name: String,
    /// Title shown in legends.
    #[serde(default)]
    pub title: String,
    /// `when:` filter expression. Parsed by [`mars_expr::parse`].
    #[serde(default)]
    pub when: Option<String>,
    /// Per-class scale window. Mirrors MapServer CLASS MINSCALEDENOM /
    /// MAXSCALEDENOM: a class is active only when the rendering scale
    /// denominator falls in `[min, max)`. When unset the class follows the
    /// layer's own scale window.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Style: either a `{ ref: name }` or an inline geometry style.
    pub style: ClassStyle,
    /// Per-class label override. When a class matches, this label fully
    /// replaces the layer-level `Layer.label` for the matched feature.
    /// Classes without a label fall back to `Layer.label`. Mirrors MapServer
    /// CLASS-level LABEL blocks.
    #[serde(default)]
    pub label: Option<LayerLabel>,
}

/// Style attachment for a class. Wire form is internally tagged on `type:`:
/// `type: ref` for a named reference, `type: inline` for a single embedded
/// style, `type: passes` for an ordered multi-pass stack. Single-pass and
/// `Inline` are equivalent on the render path; `Passes` declares an explicit
/// ordered list (fill + stroke + marker etc.) that the runtime emits one
/// `DrawOp` per pass in declared order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassStyle {
    /// Reference to a named style entry (`type: ref`, `name: <id>`).
    Ref {
        /// Name of the style entry referenced.
        name: String,
    },
    /// Inline geometry style (`type: inline`, plus all `Style` fields flat).
    /// Boxed so the enum's stack footprint stays close to the other variants;
    /// the `String` attribute field on `ScaledSize` makes a flat `Style` the
    /// dominant variant otherwise.
    Inline(Box<Style>),
    /// Ordered multi-pass stack (`type: passes`, `passes: [{...}, {...}]`).
    /// Empty list is rejected at config-load.
    Passes {
        /// Ordered list of style passes emitted per feature.
        passes: Vec<Style>,
    },
}
