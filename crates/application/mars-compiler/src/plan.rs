//! Translate a typed `Config` into a deduplicated source/layer build graph.
//!
//! - one [`SourceTask`] per distinct `(collection, band, cell)` carrying the
//!   union of attributes required by every dependent layer;
//! - one [`LayerTask`] per `(layer, band, cell)` referencing its source task
//!   by index plus the pre-compiled class table.
//!
//! The snapshot driver iterates source tasks (the unit of parallelism); for
//! each it fans out the dependent layer tasks once rows are in memory, so a
//! source cell shared between layers is fetched and materialised exactly once.

use std::collections::BTreeMap;

use mars_config::{ClassStyle, Config, Layer, ScaleWindow, SourceBinding as CfgBinding};
use mars_expr::ExprError;
use mars_grid::{BandConfig, GridError, cells_in_bbox};
use mars_source::{SourceBinding, SourceCollectionId, SourceError};
use mars_types::{Bbox, Cell, CrsCode, LayerId, ScaleBand};

use crate::class::CompiledClass;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error(transparent)]
    Grid(#[from] GridError),
    #[error(transparent)]
    Expr(#[from] ExprError),
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error("config: {0}")]
    Config(#[from] mars_config::ConfigError),
}

/// One source-side rebuild target: fetch rows once, materialise one source
/// artifact. Keyed by `(binding.collection, band, cell)` — the collection id
/// lives canonically on the binding and is exposed via [`Self::collection`].
#[derive(Debug, Clone)]
pub struct SourceTask {
    pub band: ScaleBand,
    pub cell: Cell,
    /// Binding with the *union* of attributes required across all dependent
    /// layer tasks. Identity-bearing fields (schema/table/geom/id columns,
    /// crs) must agree with every contributing layer binding.
    pub binding: SourceBinding,
    pub cell_size: f64,
    pub origin: (f64, f64),
}

impl SourceTask {
    #[must_use]
    pub fn collection(&self) -> &SourceCollectionId {
        &self.binding.collection
    }
}

/// One layer-side rebuild target. Reads from the source task at
/// [`LayerTask::source`] (an index into [`Plan::sources`]).
#[derive(Debug, Clone)]
pub struct LayerTask {
    pub layer: LayerId,
    pub band: ScaleBand,
    pub cell: Cell,
    pub source: usize,
    pub classes: Vec<CompiledClass>,
}

/// Full snapshot build plan: deduplicated source tasks + dependent layer tasks.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    pub sources: Vec<SourceTask>,
    pub layers: Vec<LayerTask>,
}

impl Plan {
    /// Group `LayerTask` indices by their source-task index. Returned vec is
    /// indexed in lockstep with `self.sources`.
    pub fn dependents_by_source(&self) -> Vec<Vec<usize>> {
        let mut out: Vec<Vec<usize>> = (0..self.sources.len()).map(|_| Vec::new()).collect();
        for (i, t) in self.layers.iter().enumerate() {
            if let Some(slot) = out.get_mut(t.source) {
                slot.push(i);
            }
        }
        out
    }
}

/// hard limit on cells a single band+binding may cover. prevents oom from
/// pathological extent / tiny cell size combinations.
pub const MAX_CELLS_PER_BAND_PER_BINDING: usize = 16_000_000;

/// build the full plan from the config.
pub fn build_plan(cfg: &Config) -> Result<Plan, PlanError> {
    let cell_sizes = cfg.cells.size_per_band_m()?;
    let band_index: BTreeMap<&str, &mars_config::Band> =
        cfg.scales.bands.iter().map(|b| (b.name.as_str(), b)).collect();
    let origin = (cfg.cells.origin[0], cfg.cells.origin[1]);
    let extent = cfg.cells.extent.unwrap_or_else(|| {
        // single-cell fallback at the origin for phase-0 in-memory tests
        Bbox::new(origin.0, origin.1, origin.0 + 1.0, origin.1 + 1.0)
    });
    let crs = cfg.source.native_crs.clone();

    // compute per-band lower bound (previous band's max_denom, or 0)
    let mut sorted_bands: Vec<_> = cfg.scales.bands.iter().collect();
    sorted_bands.sort_by_key(|b| b.max_denom);
    let mut band_mins: BTreeMap<&str, u64> = BTreeMap::new();
    let mut prev_max = 0u64;
    for band in &sorted_bands {
        band_mins.insert(band.name.as_str(), prev_max);
        prev_max = band.max_denom;
    }

    // index of (collection, band, cell.x, cell.y) -> position in plan.sources.
    // both newtypes are Arc<str>-backed, so building keys per cell is a refcount
    // bump rather than a String clone.
    let mut source_index: BTreeMap<(SourceCollectionId, ScaleBand, i64, i64), usize> = BTreeMap::new();
    let mut plan = Plan::default();

    for layer in &cfg.layers {
        let classes = compile_classes(layer)?;
        for binding in &layer.sources {
            for (band_name, &cell_size) in &cell_sizes {
                let band = band_index
                    .get(band_name.as_str())
                    .ok_or_else(|| PlanError::Invalid(format!("unknown band {band_name}")))?;
                let band_min = band_mins.get(band_name.as_str()).copied().unwrap_or(0);
                if !window_intersects(&layer.scale, band_min, band.max_denom)
                    || !window_intersects(&binding.scale, band_min, band.max_denom)
                {
                    continue;
                }
                if let Some(ref b) = binding.band
                    && b.as_str() != band_name.as_str()
                {
                    continue;
                }
                let band_cfg = BandConfig {
                    name: ScaleBand::new(band_name.as_str()),
                    max_denom: u32::try_from(band.max_denom).unwrap_or(u32::MAX),
                    origin,
                    cell_size,
                };
                let extent_for_layer = layer.bbox.unwrap_or(extent);
                let cells = cells_in_bbox(extent_for_layer, &band_cfg, MAX_CELLS_PER_BAND_PER_BINDING).map_err(|e| match e {
                    GridError::TooManyCells { .. } => PlanError::Invalid(format!(
                        "band '{band_name}' covers too many cells for layer '{}'; tighten extent or coarsen cell size",
                        layer.name
                    )),
                    other => PlanError::Grid(other),
                })?;
                if cells.is_empty() {
                    continue;
                }
                let port_binding = lower_binding(binding, &crs)?;
                let band_id = ScaleBand::new(band_name.as_str());
                for cell in cells {
                    let key = (port_binding.collection.clone(), band_id.clone(), cell.x, cell.y);
                    let source_idx = match source_index.get(&key) {
                        Some(&idx) => {
                            merge_source_binding(&mut plan.sources[idx].binding, &port_binding)?;
                            idx
                        }
                        None => {
                            let idx = plan.sources.len();
                            plan.sources.push(SourceTask {
                                band: band_id.clone(),
                                cell: cell.clone(),
                                binding: port_binding.clone(),
                                cell_size,
                                origin,
                            });
                            source_index.insert(key, idx);
                            idx
                        }
                    };
                    plan.layers.push(LayerTask {
                        layer: layer.name.clone(),
                        band: band_id.clone(),
                        cell,
                        source: source_idx,
                        classes: classes.clone(),
                    });
                }
            }
        }
    }
    Ok(plan)
}

/// Merge `incoming` into `existing` for a shared `(collection, band, cell)`.
/// Identity-bearing fields must agree; attribute lists are unioned in stable
/// order (existing first, new entries appended in incoming order).
fn merge_source_binding(existing: &mut SourceBinding, incoming: &SourceBinding) -> Result<(), PlanError> {
    if existing.from_schema != incoming.from_schema
        || existing.from_table != incoming.from_table
        || existing.geometry_column != incoming.geometry_column
        || existing.id_column != incoming.id_column
        || existing.crs != incoming.crs
    {
        return Err(PlanError::Invalid(format!(
            "two layers reference source collection {:?} with conflicting bindings; \
             schema/table/geometry_column/id_column/crs must match",
            existing.collection.as_str()
        )));
    }
    let known: std::collections::HashSet<&str> = existing.attributes.iter().map(String::as_str).collect();
    let additions: Vec<String> = incoming
        .attributes
        .iter()
        .filter(|a| !known.contains(a.as_str()))
        .cloned()
        .collect();
    existing.attributes.extend(additions);
    Ok(())
}

/// canonical-crs bbox of a single cell.
#[must_use]
pub fn cell_bbox(origin: (f64, f64), cell_size: f64, cell: &Cell) -> Bbox {
    let (ox, oy) = origin;
    let min_x = ox + cell.x as f64 * cell_size;
    let min_y = oy + cell.y as f64 * cell_size;
    Bbox::new(min_x, min_y, min_x + cell_size, min_y + cell_size)
}

pub(crate) fn window_intersects(window: &Option<ScaleWindow>, band_min: u64, band_max: u64) -> bool {
    let Some(w) = window else { return true };
    if let Some(min) = w.min
        && min >= band_max
    {
        return false;
    }
    if let Some(max) = w.max
        && max <= band_min
    {
        return false;
    }
    true
}

pub(crate) fn compile_classes(layer: &Layer) -> Result<Vec<CompiledClass>, PlanError> {
    let mut out = Vec::with_capacity(layer.classes.len());
    for (i, c) in layer.classes.iter().enumerate() {
        let when = match &c.when {
            Some(s) => Some(mars_expr::parse(s)?),
            None => None,
        };
        let style_id = match &c.style {
            ClassStyle::Ref { name } => name.clone(),
            ClassStyle::Inline(_) => format!("{}::{}", layer.name, c.name),
        };
        out.push(CompiledClass {
            name: c.name.clone(),
            when,
            style_id,
            class_index: u16::try_from(i)
                .map_err(|_| PlanError::Invalid(format!("layer {} has too many classes", layer.name)))?,
        });
    }
    Ok(out)
}

pub(crate) fn lower_binding(b: &CfgBinding, crs: &CrsCode) -> Result<SourceBinding, PlanError> {
    let (schema, table) = b.schema_table();
    let id_column = b.id_column.clone().unwrap_or_else(|| "ogc_fid".to_string());
    let collection = SourceCollectionId::new(b.from.clone());
    Ok(SourceBinding::new(
        collection,
        schema,
        table,
        b.geometry_column.clone(),
        id_column,
        b.attributes.clone(),
        crs.clone(),
    )?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_config::{Class, ClassStyle, Layer, ScaleWindow, SourceBinding as CfgBinding};
    use mars_types::{CrsCode, LayerId};

    use super::*;

    #[test]
    fn cell_bbox_negative_coords() {
        let b = cell_bbox(
            (0.0, 0.0),
            1024.0,
            &Cell {
                band: ScaleBand::new("hi"),
                x: -1,
                y: -2,
            },
        );
        assert_eq!(b.min_x, -1024.0);
        assert_eq!(b.min_y, -2048.0);
        assert_eq!(b.max_x, 0.0);
        assert_eq!(b.max_y, -1024.0);
    }

    #[test]
    fn cell_bbox_origin() {
        let b = cell_bbox(
            (100.0, 200.0),
            50.0,
            &Cell {
                band: ScaleBand::new("hi"),
                x: 0,
                y: 0,
            },
        );
        assert_eq!(b.min_x, 100.0);
        assert_eq!(b.min_y, 200.0);
        assert_eq!(b.max_x, 150.0);
        assert_eq!(b.max_y, 250.0);
    }

    #[test]
    fn window_intersects_no_window() {
        assert!(window_intersects(&None, 0, 25000));
    }

    #[test]
    fn window_intersects_min_at_threshold_rejected() {
        // band covers [0, 25000); window.min = 25000 means no overlap
        let w = ScaleWindow {
            min: Some(25000),
            max: None,
        };
        assert!(!window_intersects(&Some(w), 0, 25000));
    }

    #[test]
    fn window_intersects_min_below_threshold_accepted() {
        let w = ScaleWindow {
            min: Some(24999),
            max: None,
        };
        assert!(window_intersects(&Some(w), 0, 25000));
    }

    #[test]
    fn window_intersects_max_at_threshold_rejected() {
        // band covers [1000, 25000); window.max = 1000 means no overlap
        let w = ScaleWindow {
            min: None,
            max: Some(1000),
        };
        assert!(!window_intersects(&Some(w), 1000, 25000));
    }

    #[test]
    fn window_intersects_max_above_threshold_accepted() {
        let w = ScaleWindow {
            min: None,
            max: Some(1001),
        };
        assert!(window_intersects(&Some(w), 1000, 25000));
    }

    #[test]
    fn window_intersects_max_zero_rejected() {
        let w = ScaleWindow {
            min: None,
            max: Some(0),
        };
        assert!(!window_intersects(&Some(w), 0, 25000));
    }

    #[test]
    fn compile_classes_assigns_stable_indices() {
        let layer = Layer {
            name: LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![
                Class {
                    name: "a".into(),
                    title: String::new(),
                    when: Some("x = 1".into()),
                    style: ClassStyle::Ref { name: "s1".into() },
                },
                Class {
                    name: "b".into(),
                    title: String::new(),
                    when: Some("x = 2".into()),
                    style: ClassStyle::Ref { name: "s2".into() },
                },
            ],
            label: None,
        };
        let compiled = compile_classes(&layer).unwrap();
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].class_index, 0);
        assert_eq!(compiled[1].class_index, 1);
        assert_eq!(compiled[0].style_id, "s1");
        assert_eq!(compiled[1].style_id, "s2");
    }

    #[test]
    fn compile_classes_inline_style_namespaced() {
        let layer = Layer {
            name: LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes: vec![Class {
                name: "a".into(),
                title: String::new(),
                when: None,
                style: ClassStyle::Inline(Default::default()),
            }],
            label: None,
        };
        let compiled = compile_classes(&layer).unwrap();
        assert_eq!(compiled[0].style_id, "roads::a");
    }

    #[test]
    fn compile_classes_rejects_too_many() {
        let mut classes = Vec::new();
        for i in 0..65537 {
            classes.push(Class {
                name: format!("c{i}"),
                title: String::new(),
                when: None,
                style: ClassStyle::Ref { name: "s".into() },
            });
        }
        let layer = Layer {
            name: LayerId::new("x"),
            title: String::new(),
            abstract_: String::new(),
            kind: "line".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![],
            classes,
            label: None,
        };
        assert!(matches!(compile_classes(&layer), Err(PlanError::Invalid(_))));
    }

    #[test]
    fn lower_binding_splits_schema_table() {
        let b = CfgBinding {
            scale: None,
            band: Some("hi".into()),
            from: "myschema.mytable".into(),
            geometry_column: "geom".into(),
            id_column: Some("gid".into()),
            attributes: vec!["name".into()],
        };
        let binding = lower_binding(&b, &CrsCode::new("EPSG:25832")).unwrap();
        assert_eq!(binding.from_schema, "myschema");
        assert_eq!(binding.from_table, "mytable");
        assert_eq!(binding.id_column, "gid");
    }

    #[test]
    fn lower_binding_defaults_schema_to_public() {
        let b = CfgBinding {
            scale: None,
            band: Some("hi".into()),
            from: "mytable".into(),
            geometry_column: "geom".into(),
            id_column: None,
            attributes: vec![],
        };
        let binding = lower_binding(&b, &CrsCode::new("EPSG:25832")).unwrap();
        assert_eq!(binding.from_schema, "public");
        assert_eq!(binding.from_table, "mytable");
        assert_eq!(binding.id_column, "ogc_fid");
    }

    #[test]
    fn window_intersects_across_two_bands() {
        // bands: [0, 1000) and [1000, 5000)
        // window [500, 1500) overlaps both
        let w = ScaleWindow {
            min: Some(500),
            max: Some(1500),
        };
        assert!(window_intersects(&Some(w.clone()), 0, 1000));
        assert!(window_intersects(&Some(w), 1000, 5000));

        // window [2000, 3000) overlaps only second band
        let w2 = ScaleWindow {
            min: Some(2000),
            max: Some(3000),
        };
        assert!(!window_intersects(&Some(w2.clone()), 0, 1000));
        assert!(window_intersects(&Some(w2), 1000, 5000));
    }

    #[test]
    fn build_plan_rejects_too_many_cells() {
        use mars_config::{ArtifactCache, ArtifactStore, Artifacts, Config, ServiceMeta, Source};
        use mars_types::Bbox;
        use std::collections::BTreeMap;

        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "1m".into());

        let cfg = Config {
            service: ServiceMeta {
                name: "t".into(),
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
                store: ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                },
                cache: ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: mars_config::Scales {
                bands: vec![mars_config::Band {
                    name: "hi".into(),
                    max_denom: 25000,
                }],
            },
            cells: mars_config::Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1_000_000.0, 1_000_000.0)),
            },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![Layer {
                name: LayerId::new("roads"),
                title: String::new(),
                abstract_: String::new(),
                kind: "line".into(),
                scale: None,
                group: None,
                enable_get_feature_info: false,
                bbox: None,
                sources: vec![CfgBinding {
                    scale: None,
                    band: None,
                    from: "roads".into(),
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                }],
                classes: vec![],
                label: None,
            }],
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        };

        let err = build_plan(&cfg).unwrap_err();
        assert!(
            matches!(err, PlanError::Invalid(ref s) if s.contains("too many cells")),
            "expected Invalid with 'too many cells', got {err:?}"
        );
    }

    #[test]
    fn build_plan_honours_binding_band() {
        use mars_config::{ArtifactCache, ArtifactStore, Artifacts, Config, ServiceMeta, Source};
        use mars_types::Bbox;
        use std::collections::BTreeMap;

        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "4096m".into());
        size_per_band.insert("lo".into(), "8192m".into());

        let cfg = Config {
            service: ServiceMeta {
                name: "t".into(),
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
                store: ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                },
                cache: ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: mars_config::Scales {
                bands: vec![
                    mars_config::Band {
                        name: "hi".into(),
                        max_denom: 25000,
                    },
                    mars_config::Band {
                        name: "lo".into(),
                        max_denom: 100000,
                    },
                ],
            },
            cells: mars_config::Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1.0, 1.0)),
            },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![Layer {
                name: LayerId::new("roads"),
                title: String::new(),
                abstract_: String::new(),
                kind: "line".into(),
                scale: None,
                group: None,
                enable_get_feature_info: false,
                bbox: None,
                sources: vec![CfgBinding {
                    scale: None,
                    band: Some("lo".into()),
                    from: "roads".into(),
                    geometry_column: "geom".into(),
                    id_column: None,
                    attributes: vec![],
                }],
                classes: vec![],
                label: None,
            }],
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        };

        let plan = build_plan(&cfg).unwrap();
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.sources[0].band.as_str(), "lo");
        assert_eq!(plan.layers[0].band.as_str(), "lo");
    }

    fn make_layer(name: &str, from: &str, attrs: Vec<&str>) -> Layer {
        Layer {
            name: LayerId::new(name),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".into(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![CfgBinding {
                scale: None,
                band: Some("hi".into()),
                from: from.into(),
                geometry_column: "geom".into(),
                id_column: Some("gid".into()),
                attributes: attrs.into_iter().map(String::from).collect(),
            }],
            classes: vec![],
            label: None,
        }
    }

    fn shared_source_cfg(layers: Vec<Layer>) -> Config {
        use mars_config::{ArtifactCache, ArtifactStore, Artifacts, Config, ServiceMeta, Source};
        use mars_types::Bbox;
        use std::collections::BTreeMap;

        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "4096m".into());
        Config {
            service: ServiceMeta {
                name: "t".into(),
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
                store: ArtifactStore {
                    kind: "fs".into(),
                    endpoint: None,
                    bucket: None,
                    prefix: None,
                    path: Some("/tmp".into()),
                },
                cache: ArtifactCache {
                    path: "/tmp".into(),
                    max_size: "1GiB".into(),
                    eviction: "lru".into(),
                    trust_path_hash: false,
                },
            },
            scales: mars_config::Scales {
                bands: vec![mars_config::Band {
                    name: "hi".into(),
                    max_denom: 25_000,
                }],
            },
            cells: mars_config::Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: Some(Bbox::new(0.0, 0.0, 1.0, 1.0)),
            },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers,
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        }
    }

    #[test]
    fn build_plan_dedups_shared_source_cells_and_unions_attrs() {
        let cfg = shared_source_cfg(vec![
            make_layer("a", "public.shared", vec!["x"]),
            make_layer("b", "public.shared", vec!["y", "x"]),
        ]);
        let plan = build_plan(&cfg).unwrap();
        assert_eq!(plan.sources.len(), 1, "shared source cell deduplicated");
        assert_eq!(plan.layers.len(), 2, "one layer task per layer");
        let attrs = &plan.sources[0].binding.attributes;
        assert_eq!(attrs, &vec!["x".to_string(), "y".to_string()]);
        assert_eq!(plan.layers[0].source, 0);
        assert_eq!(plan.layers[1].source, 0);
    }

    #[test]
    fn build_plan_rejects_conflicting_bindings_for_same_collection() {
        let cfg = shared_source_cfg(vec![
            make_layer("a", "public.shared", vec![]),
            Layer {
                sources: vec![CfgBinding {
                    scale: None,
                    band: Some("hi".into()),
                    from: "public.shared".into(),
                    geometry_column: "other_geom".into(),
                    id_column: Some("gid".into()),
                    attributes: vec![],
                }],
                ..make_layer("b", "public.shared", vec![])
            },
        ]);
        let err = build_plan(&cfg).unwrap_err();
        assert!(
            matches!(err, PlanError::Invalid(ref s) if s.contains("conflicting bindings")),
            "expected conflicting-bindings error, got {err:?}"
        );
    }
}
