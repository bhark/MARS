//! plan data types. plain shapes shared by the planner submodules and
//! consumed by the rest of the compiler crate. no logic beyond the
//! `BootstrapPlan::layers_for` projection used by snapshot/rebuild.

use mars_config::{MissingPagePolicy, SimplifierKind, SourceId};
use mars_expr::{Expr, Template};
use mars_style::{LabelStyle, LabelSurvival, Placement};
use mars_types::{BindingId, CrsCode, DecimationLevel, LayerId, RasterLayerEntry};

/// One (level, decimation rules) entry on a [`BindingPlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct LevelPlan {
    pub level: DecimationLevel,
    pub vertex_tolerance_m: f64,
    pub geometry_min_size_m: f64,
    pub label_min_priority: u32,
}

/// One source binding to materialise.
#[derive(Debug, Clone, PartialEq)]
pub struct BindingPlan {
    pub binding_id: BindingId,
    /// Id of the configured source that feeds this binding. The compiler
    /// looks this up in the [`crate::SourceRegistry`] to obtain the adapter
    /// that handles `stream_rows` / `stream_rows_by_id` / `open_compile_session`.
    pub source_id: SourceId,
    /// Opaque backend-side locator passed verbatim to the source adapter via
    /// `port::SourceBinding.from`. For postgis `from:` bindings this is the
    /// table reference (`"schema.table"` or just `"table"`); for postgis
    /// `sql:` bindings it is the parenthesised inline SELECT (`"(SELECT …)"`);
    /// for vectorfile bindings it is the URI with an embedded `#format=...&source_crs=...`
    /// fragment the adapter consumes.
    pub source_table: String,
    pub geometry_field: String,
    pub id_field: Option<String>,
    pub attributes: Vec<String>,
    /// Pre-parsed binding-level filter; ANDed into the source SELECT at fetch
    /// time. Two bindings on the same table with different filters cannot
    /// share a page set, so dedup treats this as part of the binding identity.
    pub filter: Option<Expr>,
    pub native_crs: CrsCode,
    pub levels: Vec<LevelPlan>,
    pub page_size_target_bytes: u64,
    /// Encoded page-membership sidecar size threshold past which the rebuild
    /// path emits a runbook-pointing warning. Resolved from
    /// [`mars_config::SourceBinding::sidecar_size_warn_bytes`] via
    /// [`mars_config::SourceBinding::resolved_sidecar_size_warn_bytes`].
    /// Exceeding this threshold triggers a warning to consider REPLICA IDENTITY FULL.
    pub sidecar_size_warn_bytes: u64,
    /// Cadence (in incremental cycles) of the full feature-id reconciliation
    /// pass. Page-membership sidecar.
    pub reconcile_every_cycles: u32,
    /// Geometry simplifier strategy applied to every page on snapshot and
    /// rebuild. Resolved from
    /// [`mars_config::SourceBinding::resolved_simplifier`].
    pub simplifier: SimplifierKind,
    /// What to do when an incremental change event's hilbert key falls
    /// outside every page range. Resolved from
    /// [`mars_config::SourceBinding::resolved_missing_page_policy`].
    pub missing_page_policy: MissingPagePolicy,
    /// Per-binding adapter-side DSN override. Postgis-only;
    /// vector-file bindings always set this to `None`. When set, the
    /// adapter routes this binding's snapshot/rebuild queries to the
    /// override DSN's pool. Mirrors
    /// [`mars_config::SourceBinding::dsn`].
    pub dsn: Option<String>,
}

/// One pre-parsed class entry on a [`LayerPlan`]. `when` parses once at
/// plan-build time so the per-feature evaluator never reaches for the parser.
/// `style_ref` is the canonical name written into the page's StyleRefs
/// section: a `ClassStyle::Ref { name }` keeps the operator's name; an
/// inline style synthesises `<layer>__<class>` so the runtime can dereference
/// it through the published style artifact.
///
/// `label` is the per-class label override (mapfile CLASS-level LABEL).
/// When the class matches, this label fully replaces the layer-level label
/// for the feature; classes without a per-class label fall back to
/// `LayerPlan.label`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassPlan {
    pub name: String,
    pub when: Option<Expr>,
    pub style_ref: String,
    pub label: Option<LayerLabelPlan>,
}

/// Pre-parsed label spec. `text` is the parsed template; `placement` is the
/// resolved placement (the layer's `placement:` block when set, else the
/// per-geom-kind default from [`mars_style::default_placement`]).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerLabelPlan {
    pub style_ref: String,
    pub style: LabelStyle,
    pub text: Template,
    pub placement: Placement,
}

/// One layer's compile-time plan. Parsed once so snapshot/rebuild can run
/// per-feature evaluation without reparsing on every page.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerPlan {
    pub layer_id: LayerId,
    pub binding_id: BindingId,
    pub kind: String,
    pub classes: Vec<ClassPlan>,
    pub label: Option<LayerLabelPlan>,
    pub label_survival: LabelSurvival,
}

/// Full snapshot work plan: the deduplicated set of bindings the compiler
/// has to emit, plus the per-layer compile state used to fan out class /
/// label sidecar emission per page. Raster layers are materialised as
/// metadata-only [`RasterLayerEntry`] rows the publisher copies into the
/// manifest; the compiler does not fetch or stage raster bytes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BootstrapPlan {
    pub bindings: Vec<BindingPlan>,
    pub layers: Vec<LayerPlan>,
    pub raster_layers: Vec<RasterLayerEntry>,
}

impl BootstrapPlan {
    /// every layer plan that targets `binding_id`. snapshot iterates this for
    /// each (binding, level, page) so it knows which sidecars to emit.
    pub fn layers_for<'a>(&'a self, binding_id: &BindingId) -> impl Iterator<Item = &'a LayerPlan> + 'a {
        let needle = binding_id.clone();
        self.layers.iter().filter(move |l| l.binding_id == needle)
    }
}
