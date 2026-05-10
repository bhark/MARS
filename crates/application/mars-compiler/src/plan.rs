//! page enumeration plan for the snapshot compiler.
//!
//! a [`BootstrapPlan`] is the deduplicated set of bindings that the snapshot
//! will materialise. derived from a validated [`mars_config::Config`]: every
//! [`mars_config::SourceBinding`] across every layer collapses to a single
//! [`BindingPlan`] keyed by `(from, geometry_column, attributes)`. layers
//! that reference the same source see the same binding, and therefore share
//! page artifacts.
//!
//! the planner does NOT walk source rows or talk to postgres -- it only
//! decides what set of (binding, level) slices the snapshot has to emit.

use mars_config::{
    Config, DEFAULT_PAGE_SIZE_TARGET_BYTES, DecimationLevelConfig, LabelStyleAttach, Layer as CfgLayer, SimplifierKind,
};
use mars_expr::{Expr, Template, parse, parse_template};
use mars_style::{LabelStyle, LabelSurvival, Placement, default_placement};
use mars_types::{BindingId, BindingIdError, CrsCode, DecimationLevel, LayerId};

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
    /// substrate axes — multiple sources of one layer that resolve to the
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
    pub source_table: String,
    pub geometry_column: String,
    pub id_column: Option<String>,
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
}

/// One pre-parsed class entry on a [`LayerPlan`]. `when` parses once at
/// plan-build time so the per-feature evaluator never reaches for the parser.
/// `style_ref` is the canonical name written into the page's StyleRefs
/// section: a `ClassStyle::Ref { name }` keeps the operator's name; an
/// inline style synthesises `<layer>__<class>` so the runtime can dereference
/// it through the published style artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassPlan {
    pub name: String,
    pub when: Option<Expr>,
    pub style_ref: String,
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
/// label sidecar emission per page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BootstrapPlan {
    pub bindings: Vec<BindingPlan>,
    pub layers: Vec<LayerPlan>,
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
/// `(from, geometry_column, attributes)`; a binding with no `levels:`
/// declared defaults to a single level-0 (raw) entry, since the snapshot
/// always materialises at least the canonical level.
pub fn build_bootstrap_plan(cfg: &Config) -> Result<BootstrapPlan, PlanError> {
    let native_crs = cfg.source.native_crs.clone();
    let mut bindings: Vec<BindingPlan> = Vec::new();
    let mut layers: Vec<LayerPlan> = Vec::new();

    for layer in &cfg.layers {
        for binding in &layer.sources {
            let id = binding_id_for(&binding.from)?;
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
                source_table: binding.from.clone(),
                geometry_column: binding.geometry_column.clone(),
                id_column: binding.id_column.clone(),
                attributes: binding.attributes.clone(),
                filter: filter_parsed,
                native_crs: native_crs.clone(),
                levels: level_plans(binding.levels.as_deref()),
                page_size_target_bytes: binding.resolved_page_size_target(),
                sidecar_size_warn_bytes: sidecar_warn,
                reconcile_every_cycles: binding.resolved_reconcile_every_cycles(),
                simplifier: binding.resolved_simplifier(),
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

    Ok(BootstrapPlan { bindings, layers })
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
            mars_config::ClassStyle::Inline(_) => format!("{layer}__{class}", layer = layer.name, class = class.name),
        };
        classes.push(ClassPlan {
            name: class.name.clone(),
            when,
            style_ref,
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
    let (style_ref, style) = resolve_label_style(cfg, layer, &label.style);
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

fn resolve_label_style(cfg: &Config, layer: &CfgLayer, attach: &LabelStyleAttach) -> (String, LabelStyle) {
    match attach {
        LabelStyleAttach::Ref { name } => {
            let style = cfg.styles.get(name).and_then(|e| e.as_label().cloned()).unwrap_or_else(
                // config validation should reject unknown refs; fall back to a
                // safe default rather than panic if a malformed config slips
                // through.
                placeholder_label_style,
            );
            (name.clone(), style)
        }
        LabelStyleAttach::Inline(style) => (format!("{layer}__label", layer = layer.name), style.clone()),
    }
}

fn placeholder_label_style() -> LabelStyle {
    LabelStyle {
        font_family: "DejaVu Sans".into(),
        font_size: 12.0,
        fill: mars_style::Colour::rgb(0, 0, 0),
        halo: None,
        priority: 0,
        min_distance: 0.0,
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

fn ensure_consistent(existing: &BindingPlan, candidate: &BindingPlan) -> Result<(), PlanError> {
    if existing.geometry_column != candidate.geometry_column {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "geometry_column",
        });
    }
    if existing.attributes != candidate.attributes {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "attributes",
        });
    }
    if existing.id_column != candidate.id_column {
        return Err(PlanError::ConflictingBinding {
            id: existing.binding_id.clone(),
            detail: "id_column",
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

#[allow(dead_code)]
const _DEFAULT_PAGE_SIZE_USED: u64 = DEFAULT_PAGE_SIZE_TARGET_BYTES;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_config::{
        Artifacts, Band, Cells, ClassStyle, Config, Interfaces, Scales, ServiceMeta, Source, SourceBinding,
    };
    use mars_types::{Bbox, CrsCode, LayerId};
    use std::collections::BTreeMap;

    fn config_with(layers: Vec<mars_config::Layer>) -> Config {
        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "1024m".into());
        Config {
            service: ServiceMeta {
                name: "test".into(),
                ..Default::default()
            },
            source: Source {
                kind: "memory".into(),
                dsn: "memory://".into(),
                native_crs: CrsCode::new("EPSG:25832"),
                change_feed: None,
                pool: Default::default(),
            },
            artifacts: Artifacts {
                store: mars_config::ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                    allow_http: false,
                    ..Default::default()
                },
                cache: mars_config::ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: Scales {
                bands: vec![Band {
                    name: "hi".into(),
                    max_denom: 25_000,
                }],
            },
            cells: Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1_000.0, 1_000.0)),
            },
            interfaces: Interfaces::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers,
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        }
    }

    fn binding(from: &str) -> SourceBinding {
        SourceBinding {
            scale: None,
            band: None,
            max_denom: None,
            filter: None,
            from: from.into(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec!["name".into()],
            levels: None,
            page_size_target_bytes: None,
            reconcile_every_cycles: None,
            sidecar_size_warn_bytes: None,
            simplifier: None,
        }
    }

    fn layer(name: &str, sources: Vec<SourceBinding>) -> mars_config::Layer {
        mars_config::Layer {
            name: LayerId::new(name),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources,
            classes: vec![mars_config::Class {
                name: "default".into(),
                title: String::new(),
                when: None,
                scale: None,
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
        }
    }

    #[test]
    fn empty_config_yields_empty_plan() {
        let cfg = config_with(vec![]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert!(plan.bindings.is_empty());
    }

    #[test]
    fn single_binding_default_levels() {
        let cfg = config_with(vec![layer("a", vec![binding("buildings")])]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.bindings.len(), 1);
        let b = &plan.bindings[0];
        assert_eq!(b.binding_id.as_str(), "buildings");
        assert_eq!(b.source_table, "buildings");
        assert_eq!(b.geometry_column, "geom");
        assert_eq!(b.attributes, vec!["name".to_string()]);
        assert_eq!(b.native_crs.as_str(), "EPSG:25832");
        assert_eq!(b.levels.len(), 1);
        assert_eq!(b.levels[0].level, DecimationLevel::new(0));
        assert_eq!(b.page_size_target_bytes, DEFAULT_PAGE_SIZE_TARGET_BYTES);
    }

    #[test]
    fn shared_binding_dedup_across_layers() {
        let cfg = config_with(vec![
            layer("a", vec![binding("parcels")]),
            layer("b", vec![binding("parcels")]),
        ]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.bindings.len(), 1);
        assert_eq!(plan.bindings[0].binding_id.as_str(), "parcels");
    }

    #[test]
    fn two_bindings_three_levels_each() {
        let mut b1 = binding("a");
        b1.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            },
            DecimationLevelConfig {
                level: 1,
                vertex_tolerance_m: 1.0,
                geometry_min_size_m: 1.0,
                label_min_priority: 5,
            },
            DecimationLevelConfig {
                level: 2,
                vertex_tolerance_m: 4.0,
                geometry_min_size_m: 8.0,
                label_min_priority: 10,
            },
        ]);
        let b2 = binding("b");
        let cfg = config_with(vec![layer("l", vec![b1, b2])]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.bindings.len(), 2);
        assert_eq!(plan.bindings[0].levels.len(), 3);
        assert_eq!(plan.bindings[1].levels.len(), 1);
    }

    #[test]
    fn layer_plan_parses_when_clauses_and_resolves_inline_style_ref() {
        let mut b = binding("buildings");
        b.attributes = vec!["kind".into()];
        let l = mars_config::Layer {
            name: LayerId::new("bygning"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![b],
            classes: vec![
                mars_config::Class {
                    name: "main".into(),
                    title: String::new(),
                    when: Some("kind = 'main'".into()),
                    scale: None,
                    style: ClassStyle::Inline(Default::default()),
                },
                mars_config::Class {
                    name: "default".into(),
                    title: String::new(),
                    when: None,
                    scale: None,
                    style: ClassStyle::Inline(Default::default()),
                },
            ],
            label: None,
            label_survival: mars_config::LabelSurvival::Independent,
        };
        let cfg = config_with(vec![l]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.layers.len(), 1);
        let layer = &plan.layers[0];
        assert_eq!(layer.layer_id.as_str(), "bygning");
        assert_eq!(layer.binding_id.as_str(), "buildings");
        assert_eq!(layer.classes.len(), 2);
        assert!(layer.classes[0].when.is_some());
        assert!(layer.classes[1].when.is_none());
        assert_eq!(layer.classes[0].style_ref, "bygning__main");
        assert_eq!(layer.classes[1].style_ref, "bygning__default");
    }

    #[test]
    fn layers_for_filters_to_target_binding() {
        let cfg = config_with(vec![
            layer("a", vec![binding("parcels")]),
            layer("b", vec![binding("buildings")]),
        ]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        let parcels = BindingId::try_new("parcels").unwrap();
        let collected: Vec<_> = plan
            .layers_for(&parcels)
            .map(|l| l.layer_id.as_str().to_string())
            .collect();
        assert_eq!(collected, vec!["a".to_string()]);
    }

    #[test]
    fn rejects_conflicting_geometry_column() {
        let mut b1 = binding("parcels");
        let mut b2 = binding("parcels");
        b2.geometry_column = "shape".into();
        b1.geometry_column = "geom".into();
        let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
        let err = build_bootstrap_plan(&cfg).unwrap_err();
        assert!(matches!(
            err,
            PlanError::ConflictingBinding {
                detail: "geometry_column",
                ..
            }
        ));
    }

    #[test]
    fn rejects_conflicting_attributes() {
        let b1 = binding("parcels");
        let mut b2 = binding("parcels");
        b2.attributes = vec!["other".into()];
        let cfg = config_with(vec![layer("a", vec![b1]), layer("b", vec![b2])]);
        let err = build_bootstrap_plan(&cfg).unwrap_err();
        assert!(matches!(
            err,
            PlanError::ConflictingBinding {
                detail: "attributes",
                ..
            }
        ));
    }

    /// load -> validate -> propagate. exercises that per-level decimation
    /// values declared on a binding survive the full pipeline into the
    /// compiler's BindingPlan in declaration order. closes the gap noted
    /// during the decimation audit where no test covered
    /// the propagation end-to-end.
    #[test]
    fn binding_plan_carries_decimation_levels_in_order() {
        use std::path::Path;
        let mut b = binding("buildings");
        b.levels = Some(vec![
            DecimationLevelConfig {
                level: 0,
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            },
            DecimationLevelConfig {
                level: 1,
                vertex_tolerance_m: 2.5,
                geometry_min_size_m: 5.0,
                label_min_priority: 50,
            },
            DecimationLevelConfig {
                level: 2,
                vertex_tolerance_m: 10.0,
                geometry_min_size_m: 25.0,
                label_min_priority: 100,
            },
        ]);
        let mut cfg = config_with(vec![layer("l", vec![b])]);
        mars_config::validate(&mut cfg, Path::new(".")).expect("validate");
        let plan = build_bootstrap_plan(&cfg).expect("plan");
        assert_eq!(plan.bindings.len(), 1);
        let levels = &plan.bindings[0].levels;
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].level, DecimationLevel::new(0));
        assert_eq!(levels[0].vertex_tolerance_m, 0.0);
        assert_eq!(levels[0].geometry_min_size_m, 0.0);
        assert_eq!(levels[0].label_min_priority, 0);
        assert_eq!(levels[1].level, DecimationLevel::new(1));
        assert_eq!(levels[1].vertex_tolerance_m, 2.5);
        assert_eq!(levels[1].geometry_min_size_m, 5.0);
        assert_eq!(levels[1].label_min_priority, 50);
        assert_eq!(levels[2].level, DecimationLevel::new(2));
        assert_eq!(levels[2].vertex_tolerance_m, 10.0);
        assert_eq!(levels[2].geometry_min_size_m, 25.0);
        assert_eq!(levels[2].label_min_priority, 100);
    }

    /// bands are routing rules, not substrate axes. two sources of
    /// the same layer that resolve to the same binding must collapse to one
    /// LayerPlan, otherwise rebuild emits duplicate sidecars per page.
    #[test]
    fn layer_with_two_sources_same_binding_dedupes_layer_plan() {
        let mut b1 = binding("vejmidte");
        b1.band = Some("hi".into());
        let mut b2 = binding("vejmidte");
        b2.band = Some("mid".into());
        let mut cfg = config_with(vec![layer("Vejmidte", vec![b1, b2])]);
        // band: mid must exist in scales.bands or config validation would
        // reject; the plan layer doesn't care, but keep the model coherent.
        cfg.scales.bands.push(Band {
            name: "mid".into(),
            max_denom: 250_000,
        });
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.bindings.len(), 1);
        assert_eq!(plan.layers.len(), 1, "expected one LayerPlan, got {:#?}", plan.layers);
        let id = BindingId::try_new("vejmidte").unwrap();
        assert_eq!(plan.layers_for(&id).count(), 1);
    }

    #[test]
    fn three_tier_layer_produces_three_binding_plans_and_three_layer_plans() {
        let mut b0 = binding("a");
        b0.band = Some("hi".into());
        b0.max_denom = Some(8_000);
        let mut b1 = binding("b");
        b1.band = Some("hi".into());
        b1.max_denom = Some(10_000);
        let mut b2 = binding("c");
        b2.band = Some("hi".into());
        b2.max_denom = Some(25_000);
        let cfg = config_with(vec![layer("l", vec![b0, b1, b2])]);
        let plan = build_bootstrap_plan(&cfg).unwrap();
        assert_eq!(plan.bindings.len(), 3, "expected 3 distinct BindingPlans");
        assert_eq!(plan.layers.len(), 3, "expected 3 LayerPlans");
        for lp in &plan.layers {
            assert_eq!(lp.layer_id.as_str(), "l");
        }
    }

    #[test]
    fn rejects_conflicting_layer_classes() {
        let b1 = binding("parcels");
        let b2 = binding("parcels");
        let l1 = layer("shared", vec![b1]);
        let mut l2 = layer("shared", vec![b2]);
        l2.classes = vec![mars_config::Class {
            name: "other".into(),
            title: String::new(),
            when: None,
            scale: None,
            style: ClassStyle::Inline(Default::default()),
        }];
        let cfg = config_with(vec![l1, l2]);
        let err = build_bootstrap_plan(&cfg).unwrap_err();
        assert!(
            matches!(err, PlanError::ConflictingLayer { detail: "classes", .. }),
            "unexpected error: {err:?}"
        );
    }
}
