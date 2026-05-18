use mars_style::{LabelStyle, Placement};
use serde::{Deserialize, Serialize};

/// Label declaration on a layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerLabel {
    /// Reference or inline label style.
    pub style: LabelStyleAttach,
    /// Text template (`"{column}"`).
    pub text: String,
    /// Placement rules. When omitted, the layer geometry kind drives the
    /// default (see [`mars_style::default_placement`]).
    #[serde(default)]
    pub placement: Option<Placement>,
}

/// Style attachment for a label. Wire form mirrors [`super::ClassStyle`]:
/// `type: ref` plus `name`, or `type: inline` plus all `LabelStyle` fields flat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LabelStyleAttach {
    /// Reference to a named label style (`type: ref`).
    Ref {
        /// Name of the label style referenced.
        name: String,
    },
    /// Inline label style (`type: inline`).
    Inline(LabelStyle),
}
