use mars_types::CrsCode;
use serde::{Deserialize, Serialize};

use super::ScaleWindow;
use crate::ConfigError;
use crate::SourceId;
use crate::model::source::DEFAULT_SOURCE_ID;
use crate::units;

/// Default binding source-id used when the YAML omits `source:`. Matches
/// [`crate::model::source::DEFAULT_SOURCE_ID`] so single-source configs
/// (legacy singular `source:` block, no per-binding `source:` ref) resolve
/// cleanly through the multi-source pipeline.
pub fn default_binding_source_id() -> SourceId {
    SourceId::new(DEFAULT_SOURCE_ID)
}

/// Format hint for a vectorfile binding. Inferable from the URI extension
/// at translation time; explicit on the wire so binding-time errors point
/// at config, not at adapter probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorFileFormat {
    /// FlatGeobuf (`.fgb`). Cloud-native, spatially indexed.
    FlatGeobuf,
    /// GeoJSON (`.geojson` / `.json`). RFC 7946.
    GeoJson,
    /// ESRI Shapefile bundled as a single ZIP archive (`.shp.zip` / `.zip`).
    /// The archive must carry the mandatory `.shp` + `.shx` + `.dbf` triple
    /// at a shared basename; an optional `.prj` is honoured when present.
    /// One-file packaging keeps the adapter's single-URI fetch contract.
    Shapefile,
    /// OGC GeoPackage (`.gpkg`). SQLite-backed feature container. The
    /// adapter writes the fetched bytes to a tempfile so SQLite can mmap
    /// it, then iterates the configured feature table emitting OGC WKB +
    /// attribute rows.
    GeoPackage,
}

/// Source binding for a layer. Points at one of the configured
/// [`super::super::source::Source`] entries via [`Self::source`]; the
/// kind-specific payload (postgis-table, postgis-sql, vectorfile) lives
/// under [`Self::kind`] and is selected by the `kind:` discriminator on the
/// wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBinding {
    /// Identifier of the configured source that feeds this binding. Must
    /// resolve against the service-level `sources:` list. Defaults to
    /// [`DEFAULT_SOURCE_ID`] so single-source configs don't need to name
    /// their one source.
    #[serde(default = "default_binding_source_id")]
    pub source: SourceId,
    /// Kind-specific payload (table reference, inline SELECT, or
    /// vector-file URI). The wire-format discriminator is `kind:` with
    /// values `postgis_table`, `postgis_sql`, `vectorfile`.
    #[serde(flatten)]
    pub kind: BindingKind,
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
    /// Optional binding-level filter expression (mars-expr DSL). When set,
    /// the compiler ANDs this into the source SELECT so artifacts only
    /// materialise rows the filter accepts. Mirrors MapServer DATA inline
    /// subquery WHERE / SCALEToken-driven WHERE. Identifiers must be
    /// declared in `attributes` (or be `id_column`).
    #[serde(default)]
    pub filter: Option<String>,
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
    #[serde(default)]
    pub sidecar_size_warn_bytes: Option<String>,
    /// Geometry simplifier strategy applied at decimation time. `None`
    /// resolves to [`SimplifierKind::Naive`] (Douglas-Peucker per part).
    /// Kept as an enum so additional strategies can plug in without
    /// reshaping the binding surface.
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

/// Variant-specific binding payload. The wire form is internally tagged on
/// `kind:` so each YAML binding states its shape up front and serde rejects
/// malformed inputs at deserialize time rather than runtime validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingKind {
    /// PostGIS table reference. Pulls from `<schema>.<table>` (single-segment
    /// names route to `public`); change-feed compatible.
    PostgisTable {
        /// Table reference (`table` or `schema.table`).
        from: String,
        /// Geometry column on the table.
        geometry_column: String,
        /// Optional per-binding DSN override. Snapshot queries route to this
        /// DSN; logical-replication ownership stays on the source-scope
        /// `dsn`, so override bindings are snapshot-only for change-feed
        /// purposes.
        #[serde(default)]
        dsn: Option<String>,
    },
    /// Inline SELECT statement. Snapshot-only; logical-replication
    /// change-feed bindings remain table-only because pgoutput cannot route
    /// events back to an inline view. The compiler wraps the SELECT as
    /// `FROM (<sql>) AS src`.
    PostgisSql {
        /// SELECT statement driving the binding.
        sql: String,
        /// Geometry column on the SELECT projection.
        geometry_column: String,
        /// Optional per-binding DSN override. See
        /// [`BindingKind::PostgisTable::dsn`] for semantics.
        #[serde(default)]
        dsn: Option<String>,
    },
    /// Vector file behind an object-store URI (`s3://`, `gs://`, `file://`,
    /// `https://`). The decoder named in `format` reads the bytes and
    /// reprojects from `source_crs` to the source's `native_crs` before
    /// emitting WKB. Vector-file bindings own geometry extraction, so the
    /// `geometry_column` field is absent on this variant.
    Vectorfile {
        /// Object-store URI of the vector file.
        uri: String,
        /// Decoder selecting how the bytes are parsed.
        format: VectorFileFormat,
        /// Source CRS of the vector file's coordinates.
        source_crs: CrsCode,
    },
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
///
/// Kept as an enum (rather than a marker struct) so additional strategies
/// can land without reshaping the binding surface.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimplifierKind {
    /// Per-part Douglas-Peucker. The default; produces independent simplified
    /// parts per feature without considering shared edges between features.
    #[default]
    Naive,
}

impl SourceBinding {
    /// Split `from` into `(schema, table)` for a postgis-table binding.
    /// Single-segment names route to `public` to match the postgres adapter
    /// convention. Returns `None` for postgis-sql and vectorfile bindings.
    #[must_use]
    pub fn schema_table(&self) -> Option<(&str, &str)> {
        let BindingKind::PostgisTable { from, .. } = &self.kind else {
            return None;
        };
        Some(match from.split_once('.') {
            Some((s, t)) => (s, t),
            None => ("public", from.as_str()),
        })
    }

    /// Diagnostic descriptor for the binding source: the table reference, a
    /// truncated SQL snippet, or the vector-file URI. Used in validation
    /// error messages so the operator can find the offending binding
    /// regardless of source kind.
    #[must_use]
    pub fn source_descriptor(&self) -> String {
        match &self.kind {
            BindingKind::PostgisTable { from, .. } => from.clone(),
            BindingKind::PostgisSql { sql, .. } => {
                let trimmed = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                if trimmed.len() > 80 {
                    format!("sql:{}…", &trimmed[..80])
                } else {
                    format!("sql:{trimmed}")
                }
            }
            BindingKind::Vectorfile { uri, .. } => format!("uri:{uri}"),
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
