//! plan-build error type. mirrors the validate-once-then-act pattern:
//! config validation usually catches these earlier, but the planner
//! reasserts so callers that bypass `mars_config::validate` still surface
//! typed failures instead of panicking downstream.

use mars_config::SourceId;
use mars_types::{BindingId, BindingIdError, LayerId};

/// Errors emitted while building a [`super::BootstrapPlan`].
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// A binding's `from:` could not be lifted to a [`BindingId`]. usually
    /// caught at config validation; surfaced here in case a config bypasses
    /// validate.
    #[error("invalid binding id derived from {from:?}: {source}")]
    InvalidBindingId {
        /// raw `from:` value from config
        from: String,
        /// underlying validation error
        #[source]
        source: BindingIdError,
    },
    /// Two bindings with the same id have inconsistent shape (different
    /// geometry column, attribute list, or per-level decimation). v1
    /// expects every layer using the same source to declare the same
    /// shape -- otherwise the page artifacts would have to know which
    /// layer asked for them, which defeats the source/sidecar split.
    #[error("binding {id} declared with conflicting shape across layers: {detail}")]
    ConflictingBinding {
        /// binding id with conflicting declarations
        id: BindingId,
        /// short description of which field disagrees
        detail: &'static str,
    },
    /// Same `(layer_id, binding_id)` pair declared twice with diverging
    /// class / label / kind shape. bands are routing rules, not substrate
    /// axes - multiple sources of one layer that resolve to the same
    /// binding collapse to a single `LayerPlan`, which requires their
    /// per-layer shape (classes, label, kind, label_survival) to agree.
    #[error("layer {layer} on binding {binding} declared with conflicting shape: {detail}")]
    ConflictingLayer {
        /// layer name with conflicting declarations
        layer: LayerId,
        /// binding id the conflict is scoped to
        binding: BindingId,
        /// short description of which field disagrees
        detail: &'static str,
    },
    /// A class's `when:` failed to parse. config validation usually catches
    /// this; surfaced here in case a config bypasses validate.
    #[error("layer {layer} class {class:?} when: parse error: {source}")]
    ClassWhenParse {
        /// layer name
        layer: LayerId,
        /// class name within the layer
        class: String,
        /// underlying expr error
        #[source]
        source: mars_expr::ExprError,
    },
    /// A label's `text:` template failed to parse.
    #[error("layer {layer} label text: parse error: {source}")]
    LabelTemplateParse {
        /// layer name
        layer: LayerId,
        /// underlying expr error
        #[source]
        source: mars_expr::ExprError,
    },
    /// A label's `style: { name: ... }` references a style not present in
    /// `styles:`. config validation usually catches this; surfaced here in
    /// case a config bypasses validate.
    #[error("layer {layer} label references unknown label style {name:?}")]
    UnknownLabelStyleRef {
        /// layer name
        layer: LayerId,
        /// referenced style name
        name: String,
    },
    /// A binding's `filter:` failed to parse as a `when:` expression.
    #[error("binding {id} filter: parse error: {source}")]
    BindingFilterParse {
        /// binding id whose filter failed
        id: BindingId,
        /// underlying expr error
        #[source]
        source: mars_expr::ExprError,
    },
    /// A binding's `sidecar_size_warn_bytes:` literal failed to parse as a
    /// byte size. Config validation usually catches this; surfaced here in
    /// case a config bypasses validate.
    #[error("binding {id} sidecar_size_warn_bytes: parse error: {source}")]
    BindingSidecarWarnParse {
        /// binding id whose sidecar_size_warn_bytes failed
        id: BindingId,
        /// underlying config error
        #[source]
        source: mars_config::ConfigError,
    },
    /// A binding references a source id that is not declared in `sources:`.
    /// Config validation usually catches this; surfaced here in case a
    /// config bypasses validate, or the registry was built without the
    /// declared source.
    #[error("binding from {from:?} references unknown source id {source_id}")]
    UnknownSourceRef {
        /// raw binding descriptor (the value an operator would recognise)
        from: String,
        /// the missing source id
        source_id: SourceId,
    },
    /// A vector-file binding is missing a required field (`format:` or
    /// `source_crs:`). Config validation usually catches this; surfaced here
    /// in case a config bypasses validate.
    #[error("vector-file binding {from:?} missing required field: {what}")]
    IncompleteVectorFileBinding {
        /// raw binding descriptor (the URI an operator would recognise)
        from: String,
        /// name of the missing field
        what: &'static str,
    },
    /// A binding declared neither `from:` nor `sql:` nor `uri:`. Config
    /// validation rejects this; surfaced here in case a config bypasses
    /// validate.
    #[error("binding {descriptor:?} declared neither from: nor sql: nor uri:")]
    BindingSourceUnspecified {
        /// the binding's source_descriptor() for diagnostics
        descriptor: String,
    },
}
