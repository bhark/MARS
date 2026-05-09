//! Typed serde model for MARS service YAML. SPEC §5.2 - §5.5.
//!
//! Unit-suffixed scalars (`50GiB`, `4096m`, `5min`) are deserialised as
//! strings here and parsed in [`crate::units`] when accessed; the wire form
//! is preserved verbatim so a config can be round-tripped without loss.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use mars_style::{LabelStyle, LabelSurvival, Style};
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
    /// Renderer / encoder settings.
    #[serde(default)]
    pub render: Render,
    /// Compiler settings (incremental window, etc).
    #[serde(default)]
    pub compiler: Compiler,
}

/// Compiler settings. SPEC §8.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compiler {
    /// Window over which incremental change events are batched before
    /// publishing a manifest. Unit-suffixed duration (`5min`, `30s`).
    #[serde(default = "default_compiler_window")]
    pub window: String,
    /// Maximum number of source cells the snapshot driver builds concurrently.
    /// `None` resolves at runtime to `available_parallelism()` (capped by the
    /// source-side connection pool). `NonZeroUsize` rejects 0 at deserialise.
    #[serde(default)]
    pub parallel_cells: Option<NonZeroUsize>,
    /// Per-page hydrated-row working-set ceiling enforced during pass-2
    /// page assembly (rebuild and bootstrap-from-plan). Crossing this
    /// ceiling trips [`CompilerError::ScratchBudgetExceeded`].
    /// Unit-suffixed byte literal (`256MiB`).
    ///
    /// [`CompilerError::ScratchBudgetExceeded`]: https://docs.rs/mars-compiler
    #[serde(default = "default_compile_page_working_set")]
    pub compile_page_working_set_bytes: String,
    /// Hard ceiling on pass-1 page-planner allocation
    /// (`row_count × size_of::<PlanRow>()`). Crossing this ceiling trips
    /// [`CompilerError::BootstrapPlanTooLarge`] before the planner
    /// allocates beyond it. Unit-suffixed byte literal (`8GiB`).
    ///
    /// [`CompilerError::BootstrapPlanTooLarge`]: https://docs.rs/mars-compiler
    #[serde(default = "default_compile_plan_budget")]
    pub compile_plan_budget_bytes: String,
    /// Opportunistic rebalance settings (split / merge under size or
    /// bbox-dilation drift).
    #[serde(default)]
    pub rebalance: Rebalance,
}

impl Default for Compiler {
    fn default() -> Self {
        Self {
            window: default_compiler_window(),
            parallel_cells: None,
            compile_page_working_set_bytes: default_compile_page_working_set(),
            compile_plan_budget_bytes: default_compile_plan_budget(),
            rebalance: Rebalance::default(),
        }
    }
}

impl Compiler {
    /// Resolve `window` to a `Duration`.
    pub fn window_dur(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.window)
    }

    /// Resolve `compile_page_working_set_bytes` to bytes.
    pub fn compile_page_working_set(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.compile_page_working_set_bytes)
    }

    /// Resolve `compile_plan_budget_bytes` to bytes.
    pub fn compile_plan_budget(&self) -> Result<u64, ConfigError> {
        units::parse_bytes(&self.compile_plan_budget_bytes)
    }
}

fn default_compiler_window() -> String {
    "5min".to_owned()
}

fn default_compile_page_working_set() -> String {
    "256MiB".to_owned()
}

fn default_compile_plan_budget() -> String {
    "8GiB".to_owned()
}

/// Opportunistic rebalance settings. LAZARUS §Rebalance: rebalance is
/// decoupled from the hot edit path; it runs at most once per binding per
/// maintenance window or on operator command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rebalance {
    /// Whether the periodic rebalance window is active. Off by default
    /// (opportunistic-only; operator command path remains usable).
    #[serde(default)]
    pub enabled: bool,
    /// Cadence of the rebalance window. Unit-suffixed duration (`1d`, `12h`).
    #[serde(default = "default_rebalance_window")]
    pub window: String,
}

impl Default for Rebalance {
    fn default() -> Self {
        Self {
            enabled: false,
            window: default_rebalance_window(),
        }
    }
}

impl Rebalance {
    /// Resolve `window` to a `Duration`.
    pub fn window_dur(&self) -> Result<Duration, ConfigError> {
        units::parse_duration(&self.window)
    }
}

fn default_rebalance_window() -> String {
    "1d".to_owned()
}

/// PNG deflate level. Mirrors `png::Compression` so the adapter can map it
/// without depending on this crate. `Fast` is the right default for ephemeral
/// tile output: ~5-10x quicker than `Balanced` for ~10-15% larger payloads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PngCompression {
    /// No compression. Largest files, fastest encode.
    None,
    /// Lightest compression (≈ deflate level 1 via fdeflate's fast path).
    Fastest,
    /// Solid speed/ratio tradeoff suited to ephemeral tile responses.
    #[default]
    Fast,
    /// Default of the `png` crate (≈ deflate level 6).
    Balanced,
    /// Smallest output, slowest encode.
    High,
}

/// Renderer / encoder configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Render {
    /// JPEG quality, 1-100. Defaults to 85.
    #[serde(default = "default_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Total in-flight raw-pixmap memory budget across all concurrent renders,
    /// expressed as a unit-suffixed byte literal (`512MiB`). The runtime
    /// converts to a permit count of pixels (bytes / 4) and a render reserves
    /// `width * height` permits for its lifetime.
    #[serde(default = "default_pixel_budget")]
    pub pixel_budget: String,
    /// PNG deflate level. Defaults to `fast`; `balanced` matches the upstream
    /// `png` crate default if exact byte parity with older renders is needed.
    #[serde(default)]
    pub png_compression: PngCompression,
    /// Bytes-bounded LRU of decoded source-artifact geometry (SPEC §10.4).
    /// Hits skip the LEB128 varint walk for hot source artifacts, which on
    /// PostGIS-class workloads dominates per-render CPU. Expressed as a
    /// unit-suffixed byte literal (`256MiB`).
    #[serde(default = "default_decoded_geometry_cache")]
    pub decoded_geometry_cache: String,
    /// Parallel geometry emit. Splits the per-cell `cpu.emit` loop across
    /// rayon's global pool; each worker resolves its own thread-local PROJ
    /// transformer cache. Toggleable for safe rollback.
    #[serde(default)]
    pub parallel_emit: ParallelEmit,
}

/// Configuration for the parallel geometry-emit pass.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ParallelEmit {
    /// Enable parallel dispatch. When `false`, emit runs serially on the
    /// calling worker (the pre-Phase-2 path).
    #[serde(default = "default_parallel_emit_enabled")]
    pub enabled: bool,
    /// Minimum chunk size handed to each rayon worker. Below this threshold
    /// rayon coalesces work to keep dispatch overhead off the tiny-payload
    /// hot path.
    #[serde(default = "default_parallel_emit_chunk_size")]
    pub chunk_size: usize,
}

impl Default for ParallelEmit {
    fn default() -> Self {
        Self {
            enabled: default_parallel_emit_enabled(),
            chunk_size: default_parallel_emit_chunk_size(),
        }
    }
}

fn default_parallel_emit_enabled() -> bool {
    true
}

fn default_parallel_emit_chunk_size() -> usize {
    8
}

impl Default for Render {
    fn default() -> Self {
        Self {
            jpeg_quality: default_jpeg_quality(),
            pixel_budget: default_pixel_budget(),
            png_compression: PngCompression::default(),
            decoded_geometry_cache: default_decoded_geometry_cache(),
            parallel_emit: ParallelEmit::default(),
        }
    }
}

impl Render {
    /// Resolve `pixel_budget` to permit count (raw pixels). Saturates at u32::MAX.
    pub fn pixel_budget_permits(&self) -> Result<u32, ConfigError> {
        let bytes = units::parse_bytes(&self.pixel_budget)?;
        let pixels = bytes / 4;
        Ok(u32::try_from(pixels).unwrap_or(u32::MAX))
    }

    /// Resolve `decoded_geometry_cache` to a byte budget. Saturates at usize::MAX.
    pub fn decoded_geometry_cache_bytes(&self) -> Result<usize, ConfigError> {
        let bytes = units::parse_bytes(&self.decoded_geometry_cache)?;
        Ok(usize::try_from(bytes).unwrap_or(usize::MAX))
    }
}

fn default_jpeg_quality() -> u8 {
    85
}

fn default_pixel_budget() -> String {
    "512MiB".to_owned()
}

fn default_decoded_geometry_cache() -> String {
    "256MiB".to_owned()
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
    /// Font discovery for label rendering. SPEC §14.
    #[serde(default)]
    pub fonts: Fonts,
}

/// Font discovery configuration. Controls which directories the renderer
/// scans for TrueType faces, and whether the vendored DejaVu Sans fallback
/// is registered last so labels never depend on system fontconfig.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fonts {
    /// Directories to walk for `.ttf` / `.otf` faces.
    #[serde(default)]
    pub paths: Vec<String>,
    /// When true, append the vendored DejaVu Sans fallback. Defaults to true.
    #[serde(default = "default_bundle_default")]
    pub bundle_default: bool,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            bundle_default: default_bundle_default(),
        }
    }
}

fn default_bundle_default() -> bool {
    true
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
    /// Connection-pool tuning. Defaults are conservative and adapter-specific.
    #[serde(default)]
    pub pool: SourcePool,
}

/// Connection-pool tuning surface for source adapters. All fields are optional;
/// adapters fall back to library defaults when a value is not set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcePool {
    /// Maximum number of connections held by the pool.
    #[serde(default)]
    pub max_size: Option<usize>,
    /// Recycle (idle) timeout in seconds; an idle connection past this age is
    /// discarded on next checkout.
    #[serde(default)]
    pub recycle_timeout_secs: Option<u64>,
    /// Per-statement timeout in milliseconds; applied via `SET statement_timeout`
    /// on every checkout.
    #[serde(default)]
    pub statement_timeout_ms: Option<u64>,
    /// Bound on concurrent in-flight queries pipelined on a single connection
    /// when fetching a batch of cells. Adapters apply a small default when unset.
    #[serde(default)]
    pub fetch_concurrency: Option<usize>,
    /// Replication-only: max time the worker will wait for the consumer to
    /// accept a committed batch. Stalls past this budget abort the subscription
    /// so the upstream slot does not pin pg WAL indefinitely.
    #[serde(default)]
    pub batch_send_timeout_secs: Option<u64>,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Permit plaintext (non-TLS) `http://` endpoints for object stores. Off
    /// by default; required to allow `http://` so a typo in production cannot
    /// silently drop TLS. Useful for local minio/moto fixtures only.
    #[serde(default)]
    pub allow_http: bool,
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
    /// When true, the cache treats the content-hashed key path as authority
    /// and verifies each artifact only once per process via BLAKE3. Cuts
    /// hot-path cost on hits at the price of skipping bit-rot detection
    /// after the first verification.
    ///
    /// Default: true. Cache writes are atomic and content-addressed, so a
    /// per-hit rehash is safety theatre against bit-rot. Operators concerned
    /// about silent disk corruption can flip this off.
    #[serde(default = "default_trust_path_hash")]
    pub trust_path_hash: bool,
}

fn default_eviction() -> String {
    "lru".to_string()
}

fn default_trust_path_hash() -> bool {
    true
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
    /// Exclusive upper bound on the scale denominator covered by this band:
    /// the threshold itself falls into the next band.
    #[serde(rename = "max_denom_exclusive")]
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
    /// Optional service-wide extent in canonical CRS units. Phase 0 compiler
    /// uses this to enumerate cells per band; if absent, a single cell at the
    /// origin is enumerated. Phase 1 will derive this from the union of source
    /// binding extents read from the database.
    #[serde(default)]
    pub extent: Option<Bbox>,
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
    /// Optional `host:port` to bind the WMS HTTP edge on. When unset the bin
    /// falls back to `MARS_HTTP_LISTEN` and finally `0.0.0.0:8080`.
    #[serde(default)]
    pub listen: Option<String>,
    /// Maximum width or height in pixels per GetMap request. Adapter default
    /// applies when unset.
    #[serde(default)]
    pub max_image_dimension: Option<u32>,
    /// Maximum `width * height` per GetMap request. Adapter default applies
    /// when unset.
    #[serde(default)]
    pub max_pixels: Option<u64>,
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
    /// Width of the matrix in tiles. Required by OGC WMTS 1.0.0 (07-057r7
    /// §6.1) and surfaced verbatim in `Capabilities`. Defaults to 1 so the
    /// minimum-viable single-tile setup needs no boilerplate.
    #[serde(default = "one")]
    pub matrix_width: u32,
    /// Height of the matrix in tiles. See `matrix_width`.
    #[serde(default = "one")]
    pub matrix_height: u32,
}

fn one() -> u32 {
    1
}

/// Reprojection allowlist. SPEC §6.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Reprojection {
    /// Allowed CRS authority codes.
    #[serde(default)]
    pub allowlist: Vec<CrsCode>,
}

/// Style entry as seen on the YAML wire. The `type:` field discriminates
/// (SPEC §5.4: `line | polygon | point | label`); geometry kinds all share
/// the same flat shape, label has its own field set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StyleEntry {
    /// `type: label` - label glyph style.
    Label(LabelStyle),
    /// `type: line` - stroked line style.
    Line(Style),
    /// `type: polygon` - filled+stroked polygon style.
    Polygon(Style),
    /// `type: point` - point/marker style.
    Point(Style),
}

impl StyleEntry {
    /// Borrow the inner geometry style for line/polygon/point variants.
    #[must_use]
    pub fn as_geometry(&self) -> Option<&Style> {
        match self {
            Self::Line(s) | Self::Polygon(s) | Self::Point(s) => Some(s),
            Self::Label(_) => None,
        }
    }

    /// Borrow the inner label style for the `label` variant.
    #[must_use]
    pub fn as_label(&self) -> Option<&LabelStyle> {
        match self {
            Self::Label(l) => Some(l),
            _ => None,
        }
    }
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
    /// Label-survival policy across decimation levels. Default `Independent`
    /// (label retained even when geometry is pruned at the level). LAZARUS
    /// §Decimation.
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

/// Source binding for a layer. SPEC §5.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBinding {
    /// Scale window this binding is active in.
    #[serde(default)]
    pub scale: Option<ScaleWindow>,
    /// Scale band this binding routes against. SPEC §7.3, §11 Glossary —
    /// bands are routing rules, not substrate axes. At config validation,
    /// `band` is folded into `scale` as the half-open denominator interval
    /// `[prev_max, this_max)` derived from `scales.bands`, intersected with
    /// any explicit `scale` bound. The renderer's binding picker reads only
    /// `scale`; setting both `band` and a disjoint `scale` is rejected.
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
    /// Per-decimation-level decimation rules for this binding. When unset,
    /// the compiler defaults to a single level-0 (raw) materialisation.
    /// LAZARUS Phase C substrate: the snapshot emits one page set per level,
    /// pruned by `geometry_min_size_m` and simplified to `vertex_tolerance_m`.
    #[serde(default)]
    pub levels: Option<Vec<DecimationLevelConfig>>,
    /// Byte-budget target per page artifact. None resolves to the substrate
    /// default (~5 MiB).
    #[serde(default)]
    pub page_size_target_bytes: Option<u64>,
    /// Cadence (in incremental cycles) of the full-source feature-id
    /// reconciliation pass that heals drift from missed change events
    /// (slot rewinds, pgoutput gaps). LAZARUS §Page-membership sidecar.
    /// `None` resolves to the substrate default (24).
    #[serde(default)]
    pub reconcile_every_cycles: Option<u32>,
    /// Sidecar size threshold past which `REPLICA IDENTITY FULL` should be
    /// mandated for this binding. Operators see a runbook-pointing warning
    /// when the encoded sidecar exceeds this size. Unit-suffixed byte
    /// literal (`8GiB`). `None` resolves to the substrate default.
    /// LAZARUS §Bailout 4.
    #[serde(default)]
    pub sidecar_size_warn_bytes: Option<String>,
    /// Geometry simplifier strategy applied at decimation time. `None`
    /// resolves to [`SimplifierKind::Naive`] (Douglas-Peucker per part).
    /// LAZARUS Phase E line 669: the switch is wired now so the Phase 0
    /// topology-aware simplifier can plug in without further plumbing once
    /// the spike lands.
    #[serde(default)]
    pub simplifier: Option<SimplifierKind>,
}

/// Default byte-budget target per page artifact (~5 MiB).
pub const DEFAULT_PAGE_SIZE_TARGET_BYTES: u64 = 5 * 1024 * 1024;

/// Default cadence (in cycles) of the page-membership reconciliation pass.
pub const DEFAULT_RECONCILE_EVERY_CYCLES: u32 = 24;

/// Default sidecar size warning threshold (`8 GiB`). Above this the bailout
/// in LAZARUS recommends switching the binding to `REPLICA IDENTITY FULL`.
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
    /// Topology-aware shared-edge simplification (LAZARUS Phase 0 spike).
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
/// LAZARUS §244-256: each level produces a render set (geometry pruned by
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

/// Label declaration on a layer. SPEC §5.5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerLabel {
    /// Reference or inline label style.
    pub style: LabelStyleAttach,
    /// Text template (`"{column}"`).
    pub text: String,
    /// Placement rules. When omitted, the layer geometry kind drives the
    /// default (see [`mars_style::default_placement`]).
    #[serde(default)]
    pub placement: Option<mars_style::Placement>,
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
        self.poll_interval.as_deref().map(units::parse_duration).transpose()
    }
}
