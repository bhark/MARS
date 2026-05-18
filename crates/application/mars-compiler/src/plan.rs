//! page enumeration plan for the snapshot compiler.
//!
//! a [`BootstrapPlan`] is the deduplicated set of bindings that the snapshot
//! will materialise. derived from a validated [`mars_config::Config`]: every
//! [`mars_config::SourceBinding`] across every layer collapses to a single
//! [`BindingPlan`] keyed by `(from, geometry_field, attributes)`. layers
//! that reference the same source see the same binding, and therefore share
//! page artifacts.
//!
//! the planner does NOT walk source rows or talk to postgres -- it only
//! decides what set of (binding, level) slices the snapshot has to emit.

use mars_config::{
    Config, DecimationLevelConfig, LabelStyleAttach, Layer as CfgLayer, MissingPagePolicy, SimplifierKind, SourceId,
};
use mars_expr::{Expr, Template, parse, parse_template};
use mars_style::{LabelStyle, LabelSurvival, Placement, default_placement};
use mars_types::{BindingId, BindingIdError, CrsCode, DecimationLevel, LayerId, RasterLayerEntry};

/// Errors emitted while building a [`BootstrapPlan`].
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
    /// class / label / kind shape.: bands are routing rules, not
    /// substrate axes - multiple sources of one layer that resolve to the
    /// same binding collapse to a single `LayerPlan`, which requires their
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
}

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
/// per-geom-kind default from [`default_placement`]).
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

/// Build a [`BootstrapPlan`] from a validated config. dedup key is
/// `(from, geometry_field, attributes)`; a binding with no `levels:`
/// declared defaults to a single level-0 (raw) entry, since the snapshot
/// always materialises at least the canonical level.
pub fn build_bootstrap_plan(cfg: &Config) -> Result<BootstrapPlan, PlanError> {
    // index sources by id so per-binding native_crs lookup is O(log n).
    let sources_by_id: std::collections::BTreeMap<&SourceId, &mars_config::Source> =
        cfg.sources.iter().map(|s| (&s.id, s)).collect();
    let mut bindings: Vec<BindingPlan> = Vec::new();
    let mut layers: Vec<LayerPlan> = Vec::new();
    let raster_layers = build_raster_layer_entries(cfg);

    for layer in &cfg.layers {
        // raster layers are metadata-only at compile time; they have no
        // vector sources / classes / labels to enumerate. their manifest
        // entries come from build_raster_layer_entries above.
        if matches!(
            mars_style::LayerKind::parse(layer.kind.as_str()),
            Some(mars_style::LayerKind::Raster)
        ) {
            continue;
        }
        for binding in &layer.sources {
            let (source_locator, id) = resolve_binding_source(binding)?;
            let source = sources_by_id
                .get(&binding.source)
                .copied()
                .ok_or_else(|| PlanError::InvalidBindingId {
                    from: binding.source_descriptor(),
                    source: BindingIdError::Malformed {
                        id: format!("unknown source id {}", binding.source.as_str()),
                    },
                })?;
            let native_crs = source.native_crs.clone();
            let sidecar_warn =
                binding
                    .resolved_sidecar_size_warn_bytes()
                    .map_err(|_| PlanError::ConflictingBinding {
                        id: id.clone(),
                        detail: "sidecar_size_warn_bytes failed to parse",
                    })?;
            let filter_parsed = match &binding.filter {
                Some(s) => Some(parse(s).map_err(|_| PlanError::ConflictingBinding {
                    id: id.clone(),
                    detail: "filter failed to parse",
                })?),
                None => None,
            };
            let plan = BindingPlan {
                binding_id: id.clone(),
                source_id: binding.source.clone(),
                source_table: source_locator,
                geometry_field: binding.geometry_column.clone(),
                id_field: binding.id_column.clone(),
                attributes: binding.attributes.clone(),
                filter: filter_parsed,
                native_crs,
                levels: level_plans(binding.levels.as_deref()),
                page_size_target_bytes: binding.resolved_page_size_target(),
                sidecar_size_warn_bytes: sidecar_warn,
                reconcile_every_cycles: binding.resolved_reconcile_every_cycles(),
                simplifier: binding.resolved_simplifier(),
                missing_page_policy: binding.resolved_missing_page_policy(),
                dsn: binding.dsn.clone(),
            };

            if let Some(existing) = bindings.iter().find(|b| b.binding_id == id) {
                ensure_consistent(existing, &plan)?;
            } else {
                bindings.push(plan);
            }

            let layer_plan = build_layer_plan(cfg, layer, &id)?;
            if let Some(existing) = layers
                .iter()
                .find(|l| l.layer_id == layer_plan.layer_id && l.binding_id == layer_plan.binding_id)
            {
                ensure_layer_consistent(existing, &layer_plan)?;
            } else {
                layers.push(layer_plan);
            }
        }
    }

    Ok(BootstrapPlan {
        bindings,
        layers,
        raster_layers,
    })
}

/// Translate every `kind: raster` layer in `cfg` into a [`RasterLayerEntry`]
/// for the manifest. Pure / total: validation has already enforced that
/// every raster-kind layer carries a well-formed `raster:` block, so this
/// function does not return an error type. Layers without a raster block
/// are skipped.
pub fn build_raster_layer_entries(cfg: &Config) -> Vec<RasterLayerEntry> {
    cfg.layers
        .iter()
        .filter_map(|layer| {
            let raster = layer.raster.as_ref()?;
            Some(RasterLayerEntry {
                layer_id: layer.name.clone(),
                collection: raster.source.collection.clone(),
                locator: raster.source.locator.clone(),
                source_crs: raster.source.source_crs.clone(),
                tile_size: raster.source.tile_size,
                max_level: raster.source.max_level,
                opacity: raster.opacity,
            })
        })
        .collect()
}

fn build_layer_plan(cfg: &Config, layer: &CfgLayer, binding_id: &BindingId) -> Result<LayerPlan, PlanError> {
    let mut classes: Vec<ClassPlan> = Vec::with_capacity(layer.classes.len());
    for class in &layer.classes {
        let when = match &class.when {
            Some(s) => Some(parse(s).map_err(|source| PlanError::ClassWhenParse {
                layer: layer.name.clone(),
                class: class.name.clone(),
                source,
            })?),
            None => None,
        };
        let style_ref = match &class.style {
            mars_config::ClassStyle::Ref { name } => name.clone(),
            // both single-inline and multi-pass classes synthesise the same
            // per-class style name; the bin-side stylesheet builder writes
            // the passes (or single style as a one-element slice) under it.
            mars_config::ClassStyle::Inline(_) | mars_config::ClassStyle::Passes { .. } => {
                format!("{layer}__{class}", layer = layer.name, class = class.name)
            }
        };
        let label = class
            .label
            .as_ref()
            .map(|l| build_class_label_plan(cfg, layer, &class.name, l))
            .transpose()?;
        classes.push(ClassPlan {
            name: class.name.clone(),
            when,
            style_ref,
            label,
        });
    }

    let label = layer
        .label
        .as_ref()
        .map(|l| build_label_plan(cfg, layer, l))
        .transpose()?;

    Ok(LayerPlan {
        layer_id: layer.name.clone(),
        binding_id: binding_id.clone(),
        kind: layer.kind.clone(),
        classes,
        label,
        label_survival: layer.label_survival,
    })
}

fn build_label_plan(
    cfg: &Config,
    layer: &CfgLayer,
    label: &mars_config::LayerLabel,
) -> Result<LayerLabelPlan, PlanError> {
    let template = parse_template(&label.text).map_err(|source| PlanError::LabelTemplateParse {
        layer: layer.name.clone(),
        source,
    })?;
    let inline_style_ref = format!("{layer}__label", layer = layer.name);
    let (style_ref, style) = resolve_label_style(cfg, &layer.name, &inline_style_ref, &label.style)?;
    let placement = label.placement.clone().unwrap_or_else(|| {
        let kind = mars_style::LayerGeomKind::parse(layer.kind.as_str()).unwrap_or(mars_style::LayerGeomKind::Point);
        default_placement(kind)
    });
    Ok(LayerLabelPlan {
        style_ref,
        style,
        text: template,
        placement,
    })
}

fn build_class_label_plan(
    cfg: &Config,
    layer: &CfgLayer,
    class_name: &str,
    label: &mars_config::LayerLabel,
) -> Result<LayerLabelPlan, PlanError> {
    let template = parse_template(&label.text).map_err(|source| PlanError::LabelTemplateParse {
        layer: layer.name.clone(),
        source,
    })?;
    let inline_style_ref = format!("{layer}__{class_name}__label", layer = layer.name);
    let (style_ref, style) = resolve_label_style(cfg, &layer.name, &inline_style_ref, &label.style)?;
    let placement = label.placement.clone().unwrap_or_else(|| {
        let kind = mars_style::LayerGeomKind::parse(layer.kind.as_str()).unwrap_or(mars_style::LayerGeomKind::Point);
        default_placement(kind)
    });
    Ok(LayerLabelPlan {
        style_ref,
        style,
        text: template,
        placement,
    })
}

fn resolve_label_style(
    cfg: &Config,
    layer_name: &LayerId,
    inline_style_ref: &str,
    attach: &LabelStyleAttach,
) -> Result<(String, LabelStyle), PlanError> {
    match attach {
        LabelStyleAttach::Ref { name } => {
            let style = cfg
                .styles
                .get(name)
                .and_then(|e| e.as_label().cloned())
                .ok_or_else(|| PlanError::UnknownLabelStyleRef {
                    layer: layer_name.clone(),
                    name: name.clone(),
                })?;
            Ok((name.clone(), style))
        }
        LabelStyleAttach::Inline(style) => Ok((inline_style_ref.to_string(), style.clone())),
    }
}

/// Stable level plan list. an absent `levels:` config collapses to a single
/// level-0 entry with zero decimation -- preserves the canonical raw set.
fn level_plans(cfg_levels: Option<&[DecimationLevelConfig]>) -> Vec<LevelPlan> {
    match cfg_levels {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|l| LevelPlan {
                level: DecimationLevel::new(l.level),
                vertex_tolerance_m: l.vertex_tolerance_m,
                geometry_min_size_m: l.geometry_min_size_m,
                label_min_priority: l.label_min_priority,
            })
            .collect(),
        _ => vec![LevelPlan {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
        }],
    }
}

fn binding_id_for(from: &str) -> Result<BindingId, PlanError> {
    BindingId::try_new(from).map_err(|source| PlanError::InvalidBindingId {
        from: from.to_owned(),
        source,
    })
}

/// Resolve a config binding to its (locator, id) pair. Postgis table form
/// passes the `from:` string through unchanged; sql form wraps the inline
/// SELECT in parens (so the postgres adapter can splice it into `FROM (...)
/// AS s`) and derives a stable, hash-prefixed `BindingId` so equal SELECTs
/// across layers dedupe. Vectorfile form (`uri:` + `format:` + `source_crs:`)
/// embeds the format / source_crs as a `#format=...&source_crs=...` fragment
/// on the URI so the adapter sees one opaque locator and ids dedupe per
/// (uri, format, source_crs) triple.
fn resolve_binding_source(binding: &mars_config::SourceBinding) -> Result<(String, BindingId), PlanError> {
    if let Some(from) = binding.from.as_deref() {
        let id = binding_id_for(from)?;
        return Ok((from.to_owned(), id));
    }
    if let Some(sql) = binding.sql.as_deref() {
        let hash = blake3::hash(sql.as_bytes()).to_hex();
        let id_str = format!("sql_{}", &hash.as_str()[..16]);
        let id = binding_id_for(&id_str)?;
        return Ok((format!("({sql})"), id));
    }
    if let Some(uri) = binding.uri.as_deref() {
        let fmt = binding.format.ok_or_else(|| PlanError::InvalidBindingId {
            from: binding.source_descriptor(),
            source: BindingIdError::Malformed {
                id: "vectorfile binding missing format".into(),
            },
        })?;
        let source_crs = binding.source_crs.as_ref().ok_or_else(|| PlanError::InvalidBindingId {
            from: binding.source_descriptor(),
            source: BindingIdError::Malformed {
                id: "vectorfile binding missing source_crs".into(),
            },
        })?;
        let fmt_tok = match fmt {
            mars_config::VectorFileFormat::FlatGeobuf => "flat_geobuf",
            mars_config::VectorFileFormat::GeoJson => "geo_json",
            mars_config::VectorFileFormat::Shapefile => "shapefile",
            mars_config::VectorFileFormat::GeoPackage => "geo_package",
        };
        let locator = format!("{uri}#format={fmt_tok}&source_crs={}", source_crs.as_str());
        // BindingId must be path-safe; hash the locator so URIs with colons /
        // slashes still produce a valid id. dedup key matches (uri, format, source_crs).
        let hash = blake3::hash(locator.as_bytes()).to_hex();
        let id_str = format!("vf_{}", &hash.as_str()[..16]);
        let id = binding_id_for(&id_str)?;
        return Ok((locator, id));
    }
    // config validation rejects bindings with neither from: nor sql:; surface
    // a typed error in case a config bypasses validate.
    Err(PlanError::InvalidBindingId {
        from: binding.source_descriptor(),
        source: BindingIdError::Malformed {
            id: binding.source_descriptor(),
        },
    })
}

fn ensure_consistent(existing: &BindingPlan, candidate: &BindingPlan) -> Result<(), PlanError> {
    if existing.source_id != candidate.source_id {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "source_id",
        });
    }
    if existing.geometry_field != candidate.geometry_field {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "geometry_field",
        });
    }
    if existing.attributes != candidate.attributes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "attributes",
        });
    }
    if existing.id_field != candidate.id_field {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "id_field",
        });
    }
    if existing.filter != candidate.filter {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "filter",
        });
    }
    if existing.levels != candidate.levels {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "levels",
        });
    }
    if existing.page_size_target_bytes != candidate.page_size_target_bytes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "page_size_target_bytes",
        });
    }
    if existing.sidecar_size_warn_bytes != candidate.sidecar_size_warn_bytes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "sidecar_size_warn_bytes",
        });
    }
    if existing.reconcile_every_cycles != candidate.reconcile_every_cycles {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "reconcile_every_cycles",
        });
    }
    if existing.simplifier != candidate.simplifier {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "simplifier",
        });
    }
    Ok(())
}

fn ensure_layer_consistent(existing: &LayerPlan, candidate: &LayerPlan) -> Result<(), PlanError> {
    if existing.kind != candidate.kind {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "kind",
        });
    }
    if existing.classes != candidate.classes {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "classes",
        });
    }
    if existing.label != candidate.label {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "label",
        });
    }
    if existing.label_survival != candidate.label_survival {
        return Err(PlanError::ConflictingLayer {
            layer: existing.layer_id.clone(),
            binding: existing.binding_id.clone(),
            detail: "label_survival",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
