use mars_style::{LabelStyle, LabelSurvival, Placement, Style};
use mars_types::{Bbox, CrsCode, LayerId, SourceCollectionId};
use serde::{Deserialize, Serialize};

use super::service::{AuthorityRef, IdentifierRef};
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
    /// One or more source bindings. Required for vector layers; raster layers
    /// must leave this empty (their tile source lives under `raster:`).
    #[serde(default)]
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
    /// Raster layer spec. Required when `kind == "raster"`; rejected
    /// otherwise. Mutually exclusive with `sources`, `classes`, and `label`.
    #[serde(default)]
    pub raster: Option<RasterLayerSpec>,

    /// Per-layer keywords surfaced in WMS `<KeywordList>`. Empty = element
    /// omitted.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// `wms_metadataurl_*` entries surfaced as `<MetadataURL>` blocks on the
    /// layer. Each entry pairs a content type with a format and href.
    #[serde(default)]
    pub metadata_urls: Vec<MetadataUrl>,
    /// Per-layer `<AuthorityURL>` entries. Override the service-level set on a
    /// per-layer basis; an empty list keeps the service-scoped behavior in
    /// WMS 1.3.0 (root-layer inheritance).
    #[serde(default)]
    pub authorities: Vec<AuthorityRef>,
    /// Per-layer `<Identifier>` entries (1.3.0 inherits these from the root
    /// layer by default; per-layer entries override).
    #[serde(default)]
    pub identifiers: Vec<IdentifierRef>,
    /// MapServer `wms_opaque`. When true the layer is advertised as
    /// non-transparent (clients composite it as a base layer).
    #[serde(default)]
    pub opaque: bool,
    /// Per-layer advertised CRS list. None = inherit `service.advertised_crs`
    /// (in 1.3.0 layers inherit root-layer CRSes when this is empty).
    #[serde(default)]
    pub advertised_crs: Option<Vec<String>>,
    /// MapServer `wms_attribution_*` block surfaced as `<Attribution>` on the
    /// layer.
    #[serde(default)]
    pub attribution: Option<Attribution>,
    /// MapServer `ows_include_items`: which attributes flow into
    /// GetFeatureInfo (and future WFS) output. Default = `All`.
    #[serde(default)]
    pub include_items: IncludeItems,
    /// Per-operation allow/deny gating. An explicit `Some(false)` for an
    /// operation denies it for this layer; `None` falls back to default-allow
    /// (with the exception of `GetFeatureInfo`, where `enable_get_feature_info`
    /// remains the legacy opt-in default for backward compatibility).
    #[serde(default)]
    pub request_gating: RequestGating,
}

impl Layer {
    /// Resolved gating decision for `op`. `GetFeatureInfo` keeps the legacy
    /// opt-in default via [`Self::enable_get_feature_info`] when no explicit
    /// override is present; all other ops default-allow.
    #[must_use]
    pub fn permits_wms_op(&self, op: WmsOperation) -> bool {
        match op {
            WmsOperation::GetFeatureInfo => self.request_gating.allowed(op).unwrap_or(self.enable_get_feature_info),
            other => self.request_gating.allowed(other).unwrap_or(true),
        }
    }
}

/// `<MetadataURL>` entry for a layer. `type_` carries the content-spec
/// (e.g., `"ISO19115:2003"`, `"FGDC:1998"`), `format` the MIME type of the
/// linked document, and `href` the URL. Mirrors MapServer
/// `wms_metadataurl_type` / `_format` / `_href` triples.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataUrl {
    #[serde(rename = "type")]
    pub type_: String,
    pub format: String,
    pub href: String,
}

/// `<Attribution>` block for a layer. All fields optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attribution {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub online_resource: Option<String>,
    #[serde(default)]
    pub logo: Option<LogoUrl>,
}

/// `<LogoURL format="..." width="..." height="..."><OnlineResource ../></LogoURL>`
/// surfaced from MapServer `wms_attribution_logourl_*` keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogoUrl {
    pub format: String,
    pub href: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// `ows_include_items` policy controlling which attributes flow into
/// GetFeatureInfo (and future WFS) output. Default is `All`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncludeItems {
    #[serde(default)]
    pub mode: IncludeMode,
    /// Attribute names; only meaningful when `mode == Explicit`.
    #[serde(default)]
    pub names: Vec<String>,
}

impl Default for IncludeItems {
    fn default() -> Self {
        Self {
            mode: IncludeMode::All,
            names: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncludeMode {
    #[default]
    All,
    None,
    Explicit,
}

/// WMS operations subject to per-layer gating. Wire form matches the
/// MapServer `wms_enable_request` token names (case-insensitive on parse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WmsOperation {
    GetCapabilities,
    GetMap,
    GetFeatureInfo,
    GetLegendGraphic,
    GetStyles,
    DescribeLayer,
}

/// Per-operation allow/deny set. `Some(true)` allows, `Some(false)` denies,
/// `None` falls through to the layer's default-allow (or
/// `enable_get_feature_info` for GFI). Wire form: a single block with named
/// boolean keys per operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestGating {
    #[serde(default)]
    pub get_capabilities: Option<bool>,
    #[serde(default)]
    pub get_map: Option<bool>,
    #[serde(default)]
    pub get_feature_info: Option<bool>,
    #[serde(default)]
    pub get_legend_graphic: Option<bool>,
    #[serde(default)]
    pub get_styles: Option<bool>,
    #[serde(default)]
    pub describe_layer: Option<bool>,
}

impl RequestGating {
    /// Returns the explicit gating decision for `op`. `None` means no override
    /// was set in config - callers fall through to the operation's default.
    #[must_use]
    pub fn allowed(&self, op: WmsOperation) -> Option<bool> {
        match op {
            WmsOperation::GetCapabilities => self.get_capabilities,
            WmsOperation::GetMap => self.get_map,
            WmsOperation::GetFeatureInfo => self.get_feature_info,
            WmsOperation::GetLegendGraphic => self.get_legend_graphic,
            WmsOperation::GetStyles => self.get_styles,
            WmsOperation::DescribeLayer => self.describe_layer,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn bare_layer() -> Layer {
        Layer {
            name: LayerId::new("l"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: Vec::new(),
            classes: Vec::new(),
            label: None,
            label_survival: mars_style::LabelSurvival::Independent,
            raster: None,
            keywords: Vec::new(),
            metadata_urls: Vec::new(),
            authorities: Vec::new(),
            identifiers: Vec::new(),
            opaque: false,
            advertised_crs: None,
            attribution: None,
            include_items: IncludeItems::default(),
            request_gating: RequestGating::default(),
        }
    }

    #[test]
    fn default_gating_allows_getmap_and_blocks_gfi() {
        let l = bare_layer();
        // default-allow for GetMap, GetCapabilities, GetLegendGraphic etc.
        assert!(l.permits_wms_op(WmsOperation::GetMap));
        assert!(l.permits_wms_op(WmsOperation::GetCapabilities));
        assert!(l.permits_wms_op(WmsOperation::GetLegendGraphic));
        // GFI keeps the legacy opt-in default (false) when no override is set
        assert!(!l.permits_wms_op(WmsOperation::GetFeatureInfo));
    }

    #[test]
    fn enable_gfi_flips_default_gfi_gating() {
        let mut l = bare_layer();
        l.enable_get_feature_info = true;
        assert!(l.permits_wms_op(WmsOperation::GetFeatureInfo));
    }

    #[test]
    fn explicit_gating_overrides_defaults() {
        let mut l = bare_layer();
        // override GFI=true even though enable_get_feature_info=false
        l.request_gating.get_feature_info = Some(true);
        assert!(l.permits_wms_op(WmsOperation::GetFeatureInfo));
        // explicit deny for GetMap
        l.request_gating.get_map = Some(false);
        assert!(!l.permits_wms_op(WmsOperation::GetMap));
    }

    #[test]
    fn deny_get_capabilities_is_explicit() {
        let mut l = bare_layer();
        l.request_gating.get_capabilities = Some(false);
        assert!(!l.permits_wms_op(WmsOperation::GetCapabilities));
    }
}

/// Raster layer body. Carries the tile source binding plus per-layer
/// compositing knobs. The `locator` is opaque at this layer; the adapter
/// chosen by the bin interprets it (URL template, COG key, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RasterLayerSpec {
    /// Tile source binding.
    pub source: RasterSourceBinding,
    /// Per-layer opacity multiplier in `[0.0, 1.0]`. Defaults to `1.0`.
    #[serde(default = "default_raster_opacity")]
    pub opacity: f32,
}

/// Tile source binding for a raster layer. Maps the layer's collection id
/// onto a backend-side locator interpreted by whichever `RasterSource`
/// adapter the bin registers for that collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RasterSourceBinding {
    /// Logical collection identifier the bin maps to a `RasterSource` impl.
    pub collection: SourceCollectionId,
    /// Opaque backend locator (URL template, COG key, etc.).
    pub locator: String,
    /// Native CRS of the source tiles.
    pub source_crs: CrsCode,
    /// Tile edge length in pixels. Defaults to 256 (slippy-map convention).
    #[serde(default = "default_raster_tile_size")]
    pub tile_size: u32,
    /// Inclusive maximum zoom level the source publishes.
    pub max_level: u32,
}

fn default_raster_opacity() -> f32 {
    1.0
}

fn default_raster_tile_size() -> u32 {
    256
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
    /// Source table or relation. One of `from` (table) or `sql` (raw view
    /// SELECT) must be set. `from` is the common path; `sql` covers the
    /// inline-subquery bindings mapserver expresses in `DATA` blocks.
    #[serde(default)]
    pub from: Option<String>,
    /// Inline `SELECT` driving this binding. Snapshot-only - logical
    /// replication change-feed bindings remain table-only. The compiler
    /// wraps this as `FROM (<sql>) AS src`. Mutually exclusive with `from`.
    #[serde(default)]
    pub sql: Option<String>,
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
    /// Policy for change events whose hilbert key falls outside every page
    /// range (i.e. the feature's centroid lies outside the bootstrap
    /// `combined_bbox`). `None` resolves to the substrate default
    /// ([`MissingPagePolicy::Truncate`]). See [`MissingPagePolicy`] for the
    /// trade-offs.
    #[serde(default)]
    pub on_missing_page: Option<MissingPagePolicy>,
}

/// Default byte-budget target per page artifact (~5 MiB).
pub const DEFAULT_PAGE_SIZE_TARGET_BYTES: u64 = 5 * 1024 * 1024;

/// Default cadence (in cycles) of the page-membership reconciliation pass.
pub const DEFAULT_RECONCILE_EVERY_CYCLES: u32 = 24;

/// Default sidecar size warning threshold (`8 GiB`). Above this the bailout
/// recommends switching the binding to `REPLICA IDENTITY FULL`.
pub const DEFAULT_SIDECAR_SIZE_WARN_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Policy for an incremental change event whose hilbert key falls outside
/// every page range. This happens when a feature's centroid sits outside
/// the bootstrap `combined_bbox` of its binding - inserts at the edge of
/// the world, source-side coordinate drift, geometry-column reprojection
/// gone wrong.
///
/// The default is [`MissingPagePolicy::Truncate`] because it restores
/// correctness immediately. `Warn` is retained as the historical
/// behaviour for environments that can tolerate up to one reconcile cycle
/// of drift. `Fail` is for strict environments where any drift is an
/// incident.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPagePolicy {
    /// Log a warning and proceed; the next reconciliation pass will heal
    /// the binding when it scans the source.
    Warn,
    /// Escalate the affected binding to a full truncate-class rebuild this
    /// cycle. Re-derives `combined_bbox` from source and re-emits every
    /// page. Recommended default.
    #[default]
    Truncate,
    /// Return a typed [`crate::ConfigError`]-equivalent at compile time;
    /// the cycle fails and operator alarms fire.
    Fail,
}

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
    /// `public` to match the postgres adapter convention. Returns `None`
    /// when the binding is a `sql:` view rather than a table binding.
    #[must_use]
    pub fn schema_table(&self) -> Option<(&str, &str)> {
        let from = self.from.as_deref()?;
        Some(match from.split_once('.') {
            Some((s, t)) => (s, t),
            None => ("public", from),
        })
    }

    /// True when this binding is backed by an inline `sql:` SELECT.
    #[must_use]
    pub fn is_sql_binding(&self) -> bool {
        self.sql.is_some()
    }

    /// Diagnostic descriptor for the binding source: the table reference or
    /// a truncated SQL snippet. Used in validation error messages so the
    /// operator can find the offending binding regardless of source kind.
    #[must_use]
    pub fn source_descriptor(&self) -> String {
        if let Some(t) = &self.from {
            return t.clone();
        }
        if let Some(s) = &self.sql {
            let trimmed = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if trimmed.len() > 80 {
                format!("sql:{}…", &trimmed[..80])
            } else {
                format!("sql:{trimmed}")
            }
        } else {
            "<unset>".to_string()
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

    /// Resolve `on_missing_page` against the default
    /// ([`MissingPagePolicy::Truncate`]).
    #[must_use]
    pub fn resolved_missing_page_policy(&self) -> MissingPagePolicy {
        self.on_missing_page.unwrap_or_default()
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
    /// Per-class label override. When a class matches, this label fully
    /// replaces the layer-level `Layer.label` for the matched feature.
    /// Classes without a label fall back to `Layer.label`. Mirrors MapServer
    /// CLASS-level LABEL blocks.
    #[serde(default)]
    pub label: Option<LayerLabel>,
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
