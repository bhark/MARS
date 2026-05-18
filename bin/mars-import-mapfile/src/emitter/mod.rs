//! mapfile-to-YAML emitter.
//!
//! - `skeleton`: the IR the translator writes and `render` walks
//! - `style_model`: wire-shaped mirrors of `mars_style` enums + their YAML
//!   spellings
//! - `yaml`: leaf string-shape primitives
//! - `bands` / `dsn`: derived views computed by `render`
//! - `service_meta` / `layer_meta` / `styles`: per-block YAML writers
//! - `render`: the top-level orchestrator

mod bands;
mod dsn;
mod layer_meta;
mod render;
mod service_meta;
mod skeleton;
mod style_model;
mod styles;
mod yaml;

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

pub(crate) use yaml::slugify;
