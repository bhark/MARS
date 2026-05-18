//! mapfile-to-YAML emitter.
//!
//! - `skeleton`: the IR the translator writes and `render` walks
//! - `style_model`: wire-shaped mirrors of `mars_style` enums used by the IR
//! - `bands`: derived view computed by `render`
//! - `render`: skeleton -> `RenderDefinition` -> YAML orchestrator

mod bands;
mod render;
mod skeleton;
mod style_model;

pub(crate) use render::render;

pub(crate) use bands::default_bands;

pub(crate) use skeleton::{
    BindingSource, ClassSkeleton, ClassStyleAttach, IncludeItemsSkeleton, LabelSkeleton, LayerAttributionSkeleton,
    LayerGatingSkeleton, LayerOwsSkeleton, LayerSkeleton, LayerWmsSkeleton, ServiceMetaSkeleton, Skeleton,
    SourceSkeleton, VectorFileBinding,
};

pub(crate) use style_model::{
    EmitFill, EmitLinePlacement, EmitMarker, EmitNumeric, EmitStrokeGap, MarkerKind, StyleDef, SymbolDef,
};

/// slugify a name for YAML identifiers: lowercase, non-alnum -> '_'.
pub(crate) fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}
