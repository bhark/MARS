//! Build a runtime `Stylesheet` from parsed config.
//!
//! Lives here because all three bins (`mars`, `mars-diff-capture`, the e2e
//! harness in `bin/mars/tests/`) need the exact same lowering and previously
//! kept their own near-copies, which drifted.

use std::sync::Arc;

use mars_config::{ClassStyle, Config, LabelStyleAttach};
use mars_style::Stylesheet;

pub fn build_stylesheet(cfg: &Config) -> Stylesheet {
    let mut ss = Stylesheet::default();
    for (name, entry) in &cfg.styles {
        if let Some(s) = entry.as_geometry() {
            ss.geometry.insert(name.clone(), Arc::new(s.clone()));
        } else if let Some(l) = entry.as_label() {
            ss.labels.insert(name.clone(), Arc::new(l.clone()));
        }
    }
    // also collect inline class styles under `<layer>::<class>` so runtime can
    // resolve them via the same map; refs are already covered above.
    for layer in &cfg.layers {
        for class in &layer.classes {
            if let ClassStyle::Inline(s) = &class.style {
                let key = format!("{}::{}", layer.name, class.name);
                ss.geometry.insert(key, Arc::new(s.clone()));
            }
        }
        // inline label styles land under `<layer>::label` mirroring the
        // compiler's CompiledLabelSpec::style_id convention.
        if let Some(label) = &layer.label
            && let LabelStyleAttach::Inline(l) = &label.style
        {
            ss.labels.insert(format!("{}::label", layer.name), Arc::new(l.clone()));
        }
    }
    ss
}
