//! Build a runtime `Stylesheet` from parsed config.
//!
//! Lives here because multiple bins need the exact same lowering. Geometry
//! entries lower to `Arc<[Style]>`: single-pass inline classes (and the
//! line/polygon/point named entries) become a one-element slice; multi-pass
//! authoring (`ClassStyle::Passes` or `StyleEntry::Passes`) lowers to the
//! declared list in declared order.

use std::sync::Arc;

use mars_config::{ClassStyle, Config, LabelStyleAttach};
use mars_style::{Style, Stylesheet};

pub fn build_stylesheet(cfg: &Config) -> Stylesheet {
    let mut ss = Stylesheet::default();
    for (name, entry) in &cfg.styles {
        if let Some(passes) = entry.as_geometry_passes() {
            ss.geometry.insert(name.clone(), Arc::from(passes.to_vec()));
        } else if let Some(l) = entry.as_label() {
            ss.labels.insert(name.clone(), Arc::new(l.clone()));
        }
    }
    // collect inline + multi-pass class styles under `<layer>__<class>` and
    // inline label styles under `<layer>__label`, matching the synthesised
    // style_ref names the compiler writes into the page's StyleRefs section
    // (mars-compiler::plan: `format!("{layer}__{class}")`).
    for layer in &cfg.layers {
        for class in &layer.classes {
            let key = format!("{}__{}", layer.name, class.name);
            let passes: Option<Vec<Style>> = match &class.style {
                ClassStyle::Inline(s) => Some(vec![(**s).clone()]),
                ClassStyle::Passes { passes } => Some(passes.clone()),
                ClassStyle::Ref { .. } => None,
            };
            if let Some(p) = passes {
                ss.geometry.insert(key, Arc::from(p));
            }
        }
        if let Some(label) = &layer.label
            && let LabelStyleAttach::Inline(l) = &label.style
        {
            ss.labels.insert(format!("{}__label", layer.name), Arc::new(l.clone()));
        }
    }
    ss
}
