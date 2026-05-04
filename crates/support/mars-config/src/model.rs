//! Typed serde model for MARS service YAML. SPEC §5.2 - §5.5.
//!
//! Unit-suffixed scalars (`50GiB`, `4096m`, `5min`) are deserialised as
//! strings here and parsed in [`crate::units`] when accessed; the wire form
//! is preserved verbatim so a config can be round-tripped without loss.

use std::collections::BTreeMap;
use std::time::Duration;

use mars_style::{LabelStyle, Style};
use mars_types::{Bbox, CrsCode, LayerId};
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::units;

/// Top-level service configuration. SPEC §5.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Service identity and capabilities metadata.
    pub service: ServiceMeta,
    /// Source database / change-feed configuration.
    pub source: Source,
    /// Artifact store and on-disk cache settings.
    pub artifacts: Artifacts,
    /// Scale-band definitions used by the compiler.
    pub scales: Scales,
    /// Per-band cell grid configuration.
    pub cells: Cells,
    /// External interface toggles (WMS / WMTS / final tile cache).
    pub interfaces: Interfaces,
    /// Named tile-matrix-set definitions for WMTS.
    #[serde(default)]
    pub tile_matrix_sets: BTreeMap<String, TileMatrixSet>,
    /// Reprojection allowlist.
    #[serde(default)]
    pub reprojection: Reprojection,
    /// Named styles, keyed by reference name.
    #[serde(default)]
    pub styles: BTreeMap<String, StyleEntry>,
    /// Layer definitions.
    #[serde(default)]
    pub layers: Vec<Layer>,
    /// Observability settings.
    #[serde(default)]
    pub observability: Observability,
}

/// Service identity. SPEC §5.2.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceMeta {
    /// Service slug used in URLs and manifest paths.
    pub name: String,
    /// Human-readable title shown in capabilities documents.
    #[serde(default)]
    pub title: String,
    /// Long-form abstract.
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    /// Operator contact email.
    #[serde(default)]
    pub contact_email: String,
}

/// Source database configuration. SPEC §5.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Source kind discriminator (e.g. `postgis`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Database connection string.
    pub dsn: String,
    /// Native CRS reported by the source.
    pub native_crs: CrsCode,
    /// Optional change-feed configuration.
    #[serde(default)]
    pub change_feed: Option<ChangeFeed>,
}

/// Change-feed configuration. SPEC §8.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeFeed {
    /// Change-feed kind (e.g. `pgoutput`, `polling`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Logical replication publication name.
    #[serde(default)]
    pub publication: Option<String>,
    /// Logical replication slot name.
    #[serde(default)]
    pub slot: Option<String>,
    /// Polling interval for the polling fallback.
    #[serde(default)]
    pub poll_interval: Option<String>,
}

/// Artifact storage configuration. SPEC §5.2 / §10.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifacts {
    /// Long-term artifact store.
    pub store: ArtifactStore,
    /// Local on-disk cache.
    pub cache: ArtifactCache,
}

/// Long-term artifact store config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactStore {
    /// Store kind (`s3`, `fs`, ...).
    #[serde(rename = "type")]
    pub kind: String,
    /// Endpoint URL for object stores.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Bucket name for object stores.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Key prefix for object stores.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Filesystem path for `type: fs`.
    #[serde(default)]
    pub path: Option<String>,
}

/// Local artifact cache config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactCache {
    /// Cache directory.
    pub path: String,
    /// Max disk size as a unit-suffixed literal (`50GiB`).
    pub max_size: String,
    /// Eviction policy.
    #[serde(default = "default_eviction")]
    pub eviction: String,
}

fn default_eviction() -> String {
    "lru".to_string()
}

impl ArtifactCache {
    /// Resolve `max_size` to bytes.
    pub fn max_size_bytes(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.max_size)
    }
}

/// Scale-band table. SPEC §5.2 / §7.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scales {
    /// Bands ordered fine-to-coarse.
    pub bands: Vec<Band>,
}

/// Single scale band entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    /// Band name (referenced from `cells.size_per_band`).
    pub name: String,
    /// Maximum scale denominator covered by this band.
    pub max_denom: u64,
}

/// Cell grid configuration. SPEC §7.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cells {
    /// Grid kind (`regular`).
    pub grid: String,
    /// Origin in the canonical CRS.
    pub origin: [f64; 2],
    /// Per-band cell size (unit-suffixed metres).
    pub size_per_band: BTreeMap<String, String>,
}

impl Cells {
    /// Resolve `size_per_band` values to metres.
    pub fn size_per_band_m(&self) -> Result<BTreeMap<String, f64>, ConfigError> {
        self.size_per_band
            .iter()
            .map(|(k, v)| units::parse_distance_m(v).map(|d| (k.clone(), d)))
            .collect()
    }
}

/// External interface toggles.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Interfaces {
    /// WMS endpoint config.
    #[serde(default)]
    pub wms: Option<WmsConfig>,
    /// WMTS endpoint config.
    #[serde(default)]
    pub wmts: Option<WmtsConfig>,
    /// Final tile cache config.
    #[serde(default)]
    pub tile_cache: Option<TileCacheConfig>,
}

/// WMS endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WmsConfig {
    /// Whether the endpoint is mounted.
    pub enabled: bool,
    /// Supported WMS versions.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Supported MIME formats.
    #[serde(default)]
    pub formats: Vec<String>,
}

/// WMTS endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WmtsConfig {
    /// Whether the endpoint is mounted.
    pub enabled: bool,
    /// Supported WMTS versions.
    #[serde(default)]
    pub versions: Vec<String>,
    /// Tile matrix set names exposed.
    #[serde(default)]
    pub tile_matrix_sets: Vec<String>,
    /// Supported MIME formats.
    #[serde(default)]
    pub formats: Vec<String>,
}

/// Final tile cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileCacheConfig {
    /// Whether the tile cache is enabled.
    pub enabled: bool,
    /// Cache directory.
    pub path: String,
    /// Max disk size (unit-suffixed).
    pub max_size: String,
}

impl TileCacheConfig {
    /// Resolve `max_size` to bytes.
    pub fn max_size_bytes(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.max_size)
    }
}

/// Tile-matrix-set definition. SPEC §13.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileMatrixSet {
    /// CRS the matrix set is defined in.
    pub crs: CrsCode,
    /// Top-left corner in CRS units.
    pub top_left: [f64; 2],
    /// Tile pixel dimensions.
    pub tile_size: [u32; 2],
    /// Per-level definitions.
    pub levels: Vec<TileMatrixLevel>,
}

/// Single tile-matrix level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileMatrixLevel {
    /// Zoom-level index.
    pub id: u32,
    /// Scale denominator at this level.
    pub scale_denominator: f64,
}

/// Reprojection allowlist. SPEC §6.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Reprojection {
    /// Allowed CRS authority codes.
    #[serde(default)]
    pub allowlist: Vec<CrsCode>,
}

/// Style entry as seen on the YAML wire. Either a geometry style or a label
/// style; the discriminator is the `type` field (defaulting to geometry).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StyleEntry {
    /// A label-typed style (`type: label`).
    Label(LabelStyleEntry),
    /// A geometry style (line/polygon/point).
    Geometry(Style),
}

/// Label-typed style on the wire. The `type: label` discriminator routes
/// here; the rest mirrors [`mars_style::LabelStyle`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelStyleEntry {
    /// Discriminator (always `label`).
    #[serde(rename = "type")]
    pub kind: LabelKind,
    /// Inner label style fields.
    #[serde(flatten)]
    pub style: LabelStyle,
}

/// Discriminator for label-typed style entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LabelKind {
    /// `type: label`
    #[serde(rename = "label")]
    Label,
}

/// Layer definition. SPEC §5.3.
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

/// Source binding for a layer. SPEC §5.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBinding {
    /// Scale window this binding is active in.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Scale band this binding is materialised against.
    #[serde(default)]
    pub band: Option<String>,
    /// Source table or relation.
    pub from: String,
    /// Geometry column.
    pub geometry_column: String,
    /// Identifier column.
    #[serde(default)]
    pub id_column: Option<String>,
    /// Materialised attribute columns.
    #[serde(default)]
    pub attributes: Vec<String>,
}

/// Layer class. SPEC §5.3.
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
    /// Style: either a `{ ref: name }` or an inline geometry style.
    pub style: ClassStyle,
}

/// Style attachment for a class: ref or inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClassStyle {
    /// Reference to a named style.
    Ref {
        /// Name of the style entry referenced.
        #[serde(rename = "ref")]
        ref_: String,
    },
    /// Inline geometry style.
    Inline(Style),
}

/// Label declaration on a layer. SPEC §5.5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerLabel {
    /// Reference or inline label style.
    pub style: LabelStyleAttach,
    /// Text template (`"{column}"`).
    pub text: String,
    /// Placement rules.
    #[serde(default)]
    pub placement: Option<serde_yml::Value>,
}

/// Style attachment for a label: ref or inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelStyleAttach {
    /// Reference to a named label style.
    Ref {
        /// Name of the label style referenced.
        #[serde(rename = "ref")]
        ref_: String,
    },
    /// Inline label style.
    Inline(LabelStyle),
}

/// Observability settings. SPEC §15.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Observability {
    /// `info`, `debug`, ...
    #[serde(default)]
    pub log_level: Option<String>,
    /// `json` or `text`.
    #[serde(default)]
    pub log_format: Option<String>,
    /// Prometheus listen address.
    #[serde(default)]
    pub metrics_listen: Option<String>,
    /// OTLP tracing config.
    #[serde(default)]
    pub tracing: Option<TracingConfig>,
}

/// OTLP tracing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Tracing kind (`otlp`).
    #[serde(rename = "type")]
    pub kind: String,
    /// OTLP collector endpoint.
    pub endpoint: String,
    /// Sample rate.
    #[serde(default)]
    pub sample_rate: Option<f64>,
}

impl ChangeFeed {
    /// Resolve `poll_interval` to a `Duration` if set.
    pub fn poll_interval_dur(&self) -> Result<Option<Duration>, ConfigError> {
        self.poll_interval
            .as_deref()
            .map(units::parse_duration)
            .transpose()
    }
}
