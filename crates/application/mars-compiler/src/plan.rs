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

use mars_config::{Config, DEFAULT_PAGE_SIZE_TARGET_BYTES, DecimationLevelConfig};
use mars_types::{BindingId, BindingIdError, CrsCode, DecimationLevel};

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
    pub native_crs: CrsCode,
    pub levels: Vec<LevelPlan>,
    pub page_size_target_bytes: u64,
}

/// Full snapshot work plan: the deduplicated set of bindings the compiler
/// has to emit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BootstrapPlan {
    pub bindings: Vec<BindingPlan>,
}

/// Build a [`BootstrapPlan`] from a validated config. dedup key is
/// `(from, geometry_column, attributes)`; a binding with no `levels:`
/// declared defaults to a single level-0 (raw) entry, since the snapshot
/// always materialises at least the canonical level.
pub fn build_bootstrap_plan(cfg: &Config) -> Result<BootstrapPlan, PlanError> {
    let native_crs = cfg.source.native_crs.clone();
    let mut bindings: Vec<BindingPlan> = Vec::new();

    for layer in &cfg.layers {
        for binding in &layer.sources {
            let id = binding_id_for(&binding.from)?;
            let levels = level_plans(binding.levels.as_deref());
            let plan = BindingPlan {
                binding_id: id.clone(),
                source_table: binding.from.clone(),
                geometry_column: binding.geometry_column.clone(),
                id_column: binding.id_column.clone(),
                attributes: binding.attributes.clone(),
                native_crs: native_crs.clone(),
                levels,
                page_size_target_bytes: binding.resolved_page_size_target(),
            };

            if let Some(existing) = bindings.iter().find(|b| b.binding_id == id) {
                ensure_consistent(existing, &plan)?;
                continue;
            }
            bindings.push(plan);
        }
    }

    Ok(BootstrapPlan { bindings })
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
            from: from.into(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec!["name".into()],
            levels: None,
            page_size_target_bytes: None,
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
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
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
}
