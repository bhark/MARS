use mars_style::{LabelStyle, LabelSurvival, Placement, Style};
use mars_types::{Bbox, LayerId};
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Layer definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    /// Stable layer identifier.
    pub name: LayerId,
    /// Human-readable layer title.
    #[serde(default)]
    pub title: String,
    /// Long-form abstract.
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    /// Geometry kind (`polygon`, `line`, `point`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Layer-wide scale window.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Optional flat group string.
    #[serde(default)]
    pub group: Option<String>,
    /// Whether GFI is permitted on this layer.
    #[serde(default)]
    pub enable_get_feature_info: bool,
    /// Optional layer-wide bounding-box constraint.
    #[serde(default)]
    pub bbox: Option<Bbox>,
    /// One or more source bindings.
    pub sources: Vec<SourceBinding>,
    /// Class list, top-down first-match-wins.
    #[serde(default)]
    pub classes: Vec<Class>,
    /// Optional label declaration.
    #[serde(default)]
    pub label: Option<LayerLabel>,
    /// Label-survival policy across decimation levels. Default `Independent`
    /// (label retained even when geometry is pruned at the level).
    #[serde(default)]
    pub label_survival: LabelSurvival,
}

/// Half-open scale window with denominator bounds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScaleWindow {
    /// Inclusive lower bound on scale denominator.
    #[serde(default)]
    pub min: Option<u64>,
    /// Exclusive upper bound on scale denominator.
    #[serde(default)]
    pub max: Option<u64>,
}

/// Source binding for a layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBinding {
    /// Scale window this binding is active in.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Scale band this binding routes against. Bands are routing rules, not
    /// substrate axes. At config validation,
    /// `band` is folded into `scale` as the half-open denominator interval
    /// `[prev_max, this_max)` derived from `scales.bands`, intersected with
    /// any explicit `scale` bound. The renderer's binding picker reads only
    /// `scale`; setting both `band` and a disjoint `scale` is rejected.
    #[serde(default)]
    pub band: Option<String>,
    /// Exclusive upper bound on scale denominator for this tier within its
    /// band. When multiple sources share the same `band`, they form an ordered
    /// tier-set sorted by `max_denom_exclusive` ascending. The effective
    /// half-open window per tier is `[prev_max, this_max)` intersected with
    /// the band window and any explicit `scale`. Omit on the last tier to
    /// inherit the band cap. A single source with no `max_denom_exclusive`
    /// covers the whole band (back-compat shorthand).
    #[serde(default, rename = "max_denom_exclusive")]
    pub max_denom: Option<u64>,
    /// Source table or relation.
    pub from: String,
    /// Optional binding-level filter expression (mars-expr DSL). When set,
    /// the compiler ANDs this into the source SELECT so artifacts only
    /// materialise rows the filter accepts. Mirrors MapServer DATA inline
    /// subquery WHERE / SCALEToken-driven WHERE. Identifiers must be
    /// declared in `attributes` (or be `id_column`).
    #[serde(default)]
    pub filter: Option<String>,
    /// Geometry column.
    pub geometry_column: String,
    /// Identifier column.
    #[serde(default)]
    pub id_column: Option<String>,
    /// Materialised attribute columns.
    #[serde(default)]
    pub attributes: Vec<String>,
    /// Per-decimation-level decimation rules for this binding. When unset,
    /// the compiler defaults to a single level-0 (raw) materialisation.
    /// The snapshot emits one page set per level,
    /// pruned by `geometry_min_size_m` and simplified to `vertex_tolerance_m`.
    #[serde(default)]
    pub levels: Option<Vec<DecimationLevelConfig>>,
    /// Byte-budget target per page artifact. None resolves to the substrate
    /// default (~5 MiB).
    #[serde(default)]
    pub page_size_target_bytes: Option<u64>,
    /// Cadence (in incremental cycles) of the full-source feature-id
    /// reconciliation pass that heals drift from missed change events
    /// (slot rewinds, pgoutput gaps). Page-membership sidecar.
    /// `None` resolves to the substrate default (24).
    #[serde(default)]
    pub reconcile_every_cycles: Option<u32>,
    /// Sidecar size threshold past which `REPLICA IDENTITY FULL` should be
    /// mandated for this binding. Operators see a runbook-pointing warning
    /// when the encoded sidecar exceeds this size. Unit-suffixed byte
    /// literal (`8GiB`). `None` resolves to the substrate default.
    /// Exceeding this threshold triggers a warning to consider REPLICA IDENTITY FULL.
    #[serde(default)]
    pub sidecar_size_warn_bytes: Option<String>,
    /// Geometry simplifier strategy applied at decimation time. `None`
    /// resolves to [`SimplifierKind::Naive`] (Douglas-Peucker per part).
    /// The switch is wired so the topology-aware simplifier can plug in
    /// without further plumbing once the spike lands.
    #[serde(default)]
    pub simplifier: Option<SimplifierKind>,
}

/// Default byte-budget target per page artifact (~5 MiB).
pub const DEFAULT_PAGE_SIZE_TARGET_BYTES: u64 = 5 * 1024 * 1024;

/// Default cadence (in cycles) of the page-membership reconciliation pass.
pub const DEFAULT_RECONCILE_EVERY_CYCLES: u32 = 24;

/// Default sidecar size warning threshold (`8 GiB`). Above this the bailout
/// recommends switching the binding to `REPLICA IDENTITY FULL`.
pub const DEFAULT_SIDECAR_SIZE_WARN_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Geometry simplifier strategy. The strategy is per-binding because it
/// reflects *how* simplification is performed; per-level *aggressiveness*
/// is already controlled by [`DecimationLevelConfig::vertex_tolerance_m`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimplifierKind {
    /// Per-part Douglas-Peucker. The default; produces independent simplified
    /// parts per feature without considering shared edges between features.
    #[default]
    Naive,
    /// Topology-aware shared-edge simplification (spike).
    /// Currently unimplemented; selecting this variant is rejected at
    /// config validation with [`ConfigError::Invalid`].
    TopologyAware,
}

impl SourceBinding {
    /// Split `from` into `(schema, table)`. Single-segment names route to
    /// `public` to match the postgres adapter convention.
    #[must_use]
    pub fn schema_table(&self) -> (&str, &str) {
        match self.from.split_once('.') {
            Some((s, t)) => (s, t),
            None => ("public", self.from.as_str()),
        }
    }

    /// Resolve `page_size_target_bytes` against the substrate default.
    #[must_use]
    pub fn resolved_page_size_target(&self) -> u64 {
        self.page_size_target_bytes.unwrap_or(DEFAULT_PAGE_SIZE_TARGET_BYTES)
    }

    /// Resolve `reconcile_every_cycles` against the substrate default.
    #[must_use]
    pub fn resolved_reconcile_every_cycles(&self) -> u32 {
        self.reconcile_every_cycles.unwrap_or(DEFAULT_RECONCILE_EVERY_CYCLES)
    }

    /// Resolve `sidecar_size_warn_bytes` against the substrate default. Errors
    /// if the literal cannot be parsed.
    pub fn resolved_sidecar_size_warn_bytes(&self) -> Result<u64, ConfigError> {
        match &self.sidecar_size_warn_bytes {
            Some(s) => units::parse_bytes(s),
            None => Ok(DEFAULT_SIDECAR_SIZE_WARN_BYTES),
        }
    }

    /// Resolve `simplifier` against the default ([`SimplifierKind::Naive`]).
    #[must_use]
    pub fn resolved_simplifier(&self) -> SimplifierKind {
        self.simplifier.unwrap_or_default()
    }
}

/// Per-decimation-level rules driving page emission for one binding.
/// Each level produces a render set (geometry pruned by
/// `geometry_min_size_m`, simplified to `vertex_tolerance_m`) and a label
/// set (candidates retained at or above `label_min_priority`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecimationLevelConfig {
    /// Decimation level index. Level 0 is the raw (canonical) materialisation.
    pub level: u8,
    /// Douglas-Peucker tolerance in canonical CRS units (metres for the
    /// metric CRSes mars-runtime requires).
    pub vertex_tolerance_m: f64,
    /// Drop features whose bbox-diagonal is below this threshold at this level.
    pub geometry_min_size_m: f64,
    /// Retain label candidates at or above this priority at this level.
    pub label_min_priority: u32,
}

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
}

/// Style attachment for a class. Wire form is internally tagged on `type:`:
/// `type: ref` for a named reference, `type: inline` for an embedded style.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassStyle {
    /// Reference to a named style entry (`type: ref`, `name: <id>`).
    Ref {
        /// Name of the style entry referenced.
        name: String,
    },
    /// Inline geometry style (`type: inline`, plus all `Style` fields flat).
    Inline(Style),
}

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

/// Style attachment for a label. Wire form mirrors [`ClassStyle`]:
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
