//! Translate a typed `Config` into a flat list of per-binding `BuildTask`s.
//!
//! one task = one (layer, band, binding) triple plus the cell list and the
//! pre-parsed class table. the snapshot driver consumes these directly.

use std::collections::BTreeMap;

use mars_config::{Band, ClassStyle, Config, Layer, ScaleWindow, SourceBinding as CfgBinding};
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

/// One unit of compiler work for the snapshot driver.
#[derive(Debug, Clone)]
pub struct BuildTask {
    pub layer: LayerId,
    pub band: ScaleBand,
    pub binding: SourceBinding,
    pub cells: Vec<Cell>,
    pub cell_size: f64,
    pub origin: (f64, f64),
    pub classes: Vec<CompiledClass>,
}

/// build the full plan from the config.
pub fn build_plan(cfg: &Config) -> Result<Vec<BuildTask>, PlanError> {
    let cell_sizes = cfg.cells.size_per_band_m()?;
    let band_index: BTreeMap<&str, &Band> = cfg.scales.bands.iter().map(|b| (b.name.as_str(), b)).collect();
    let origin = (cfg.cells.origin[0], cfg.cells.origin[1]);
    let extent = cfg.cells.extent.unwrap_or_else(|| {
        // single-cell fallback at the origin for phase-0 in-memory tests
        Bbox::new(origin.0, origin.1, origin.0 + 1.0, origin.1 + 1.0)
    });
    let crs = cfg.source.native_crs.clone();

    let mut tasks = Vec::new();
    for layer in &cfg.layers {
        let classes = compile_classes(layer)?;
        for binding in &layer.sources {
            for (band_name, &cell_size) in &cell_sizes {
                let band = band_index
                    .get(band_name.as_str())
                    .ok_or_else(|| PlanError::Invalid(format!("unknown band {band_name}")))?;
                if !window_intersects(&layer.scale, band) || !window_intersects(&binding.scale, band) {
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
                let cells = cells_in_bbox(extent_for_layer, &band_cfg, usize::MAX)?;
                if cells.is_empty() {
                    continue;
                }
                let port_binding = lower_binding(binding, &crs)?;
                tasks.push(BuildTask {
                    layer: layer.name.clone(),
                    band: ScaleBand::new(band_name.as_str()),
                    binding: port_binding,
                    cells,
                    cell_size,
                    origin,
                    classes: classes.clone(),
                });
            }
        }
    }
    Ok(tasks)
}

/// canonical-crs bbox of a single cell.
#[must_use]
pub fn cell_bbox(origin: (f64, f64), cell_size: f64, cell: &Cell) -> Bbox {
    let (ox, oy) = origin;
    let min_x = ox + cell.x as f64 * cell_size;
    let min_y = oy + cell.y as f64 * cell_size;
    Bbox::new(min_x, min_y, min_x + cell_size, min_y + cell_size)
}

pub(crate) fn window_intersects(window: &Option<ScaleWindow>, band: &Band) -> bool {
    // a band spans [prev_max .. band.max_denom). without prev_max here we
    // accept anything overlapping (0, band.max_denom). good enough for phase-0
    // since the test config has one band.
    let Some(w) = window else { return true };
    let band_max = band.max_denom;
    if let Some(min) = w.min
        && min >= band_max
    {
        return false;
    }
    if let Some(max) = w.max
        && max == 0
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
    // `from` is `schema.table`. tolerate single-segment names by routing to
    // the public schema (matches postgres adapter convention).
    let (schema, table) = match b.from.split_once('.') {
        Some((s, t)) => (s.to_string(), t.to_string()),
        None => ("public".to_string(), b.from.clone()),
    };
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
        let band = Band {
            name: "hi".into(),
            max_denom: 25000,
        };
        assert!(window_intersects(&None, &band));
    }

    #[test]
    fn window_intersects_min_at_threshold_rejected() {
        // band covers [0, 25000); window.min = 25000 means no overlap
        let band = Band {
            name: "hi".into(),
            max_denom: 25000,
        };
        let w = ScaleWindow {
            min: Some(25000),
            max: None,
        };
        assert!(!window_intersects(&Some(w), &band));
    }

    #[test]
    fn window_intersects_min_below_threshold_accepted() {
        let band = Band {
            name: "hi".into(),
            max_denom: 25000,
        };
        let w = ScaleWindow {
            min: Some(24999),
            max: None,
        };
        assert!(window_intersects(&Some(w), &band));
    }

    #[test]
    fn window_intersects_max_zero_rejected() {
        let band = Band {
            name: "hi".into(),
            max_denom: 25000,
        };
        let w = ScaleWindow {
            min: None,
            max: Some(0),
        };
        assert!(!window_intersects(&Some(w), &band));
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
        };

        let tasks = build_plan(&cfg).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].band.as_str(), "lo");
    }
}
