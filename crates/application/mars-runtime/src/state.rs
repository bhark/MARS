//! immutable per-snapshot runtime state. built once per manifest version.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use hashbrown::{Equivalent, HashMap};
use mars_config::Config;
use mars_grid::BandConfig;
use mars_style::Stylesheet;
use mars_types::{ArtifactEntry, CrsCode, EmptyLayerCell, LayerId, Manifest, ScaleBand};

use crate::RuntimeError;
use crate::key::{ParsedKey, parse};

/// composite indexing key for layer artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerCellKey {
    pub layer: LayerId,
    pub band: ScaleBand,
    pub x: i64,
    pub y: i64,
}

/// composite indexing key for source artifacts. collection name is kept as
/// `Arc<str>` rather than the port-side `SourceCollectionId` wrapper - the
/// runtime only needs cheap-clone equality semantics here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCellKey {
    pub collection: Arc<str>,
    pub band: ScaleBand,
    pub x: i64,
    pub y: i64,
}

/// borrowed view into a `LayerCellKey` for zero-allocation hashmap lookup.
#[derive(Debug)]
pub struct LayerCellRef<'a> {
    pub layer: &'a str,
    pub band: &'a str,
    pub x: i64,
    pub y: i64,
}

/// borrowed view into a `SourceCellKey` for zero-allocation hashmap lookup.
#[derive(Debug)]
pub struct SourceCellRef<'a> {
    pub collection: &'a str,
    pub band: &'a str,
    pub x: i64,
    pub y: i64,
}

// hash via the borrowed view so owned and ref forms produce identical hashes.
impl Hash for LayerCellKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.layer.as_str().hash(h);
        self.band.as_str().hash(h);
        self.x.hash(h);
        self.y.hash(h);
    }
}

impl Hash for LayerCellRef<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.layer.hash(h);
        self.band.hash(h);
        self.x.hash(h);
        self.y.hash(h);
    }
}

impl Equivalent<LayerCellKey> for LayerCellRef<'_> {
    fn equivalent(&self, key: &LayerCellKey) -> bool {
        self.x == key.x && self.y == key.y && self.layer == key.layer.as_str() && self.band == key.band.as_str()
    }
}

impl Hash for SourceCellKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (*self.collection).hash(h);
        self.band.as_str().hash(h);
        self.x.hash(h);
        self.y.hash(h);
    }
}

impl Hash for SourceCellRef<'_> {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.collection.hash(h);
        self.band.hash(h);
        self.x.hash(h);
        self.y.hash(h);
    }
}

impl Equivalent<SourceCellKey> for SourceCellRef<'_> {
    fn equivalent(&self, key: &SourceCellKey) -> bool {
        self.x == key.x && self.y == key.y && self.collection == &*key.collection && self.band == key.band.as_str()
    }
}

/// discriminant for layer-index lookups: a cell is either backed by a real
/// artifact or is an explicit empty marker.
#[derive(Debug, Clone)]
pub enum LayerCellState {
    Present(ArtifactEntry),
    Empty,
}

/// runtime state — pure data, no I/O. cheap to share across requests behind `Arc`.
#[derive(Debug)]
pub struct RuntimeState {
    /// canonical CRS for this service. plans must match.
    pub canonical_crs: CrsCode,
    /// scale bands, ordered fine-to-coarse (ascending max_denom).
    pub bands: Vec<BandConfig>,
    /// declared layer order (used to compose draw ops top-down).
    pub layer_order: Vec<LayerId>,
    /// compiled stylesheet (geometry + label styles by name).
    pub stylesheet: Stylesheet,
    /// active manifest snapshot.
    pub manifest: Manifest,
    /// `(layer, band, cell) -> present artifact or empty marker`.
    pub layer_index: HashMap<LayerCellKey, LayerCellState>,
    /// `(collection, band, cell) -> artifact entry`.
    pub source_index: HashMap<SourceCellKey, ArtifactEntry>,
}

impl RuntimeState {
    /// build runtime state from validated `Config` and a manifest snapshot.
    /// indices are derived by parsing manifest keys; the manifest is the
    /// single source of truth for what artifacts exist.
    pub fn from_config_and_manifest(
        config: &Config,
        stylesheet: Stylesheet,
        manifest: Manifest,
    ) -> Result<Self, RuntimeError> {
        let canonical_crs = config.source.native_crs.clone();
        let bands = build_bands(config)?;
        let layer_order = config.layers.iter().map(|l| l.name.clone()).collect();

        let mut layer_index = HashMap::with_capacity(manifest.layer_artifacts.len() + manifest.empty_layer_cells.len());
        for entry in &manifest.layer_artifacts {
            match parse(&entry.key)? {
                ParsedKey::Layer { layer, cell } => {
                    let key = LayerCellKey {
                        layer,
                        band: cell.band,
                        x: cell.x,
                        y: cell.y,
                    };
                    if layer_index.contains_key(&key) {
                        return Err(RuntimeError::BadKey {
                            key: entry.key.to_string(),
                            reason: "duplicate (layer, band, cell) in layer_artifacts".into(),
                        });
                    }
                    layer_index.insert(key, LayerCellState::Present(entry.clone()));
                }
                ParsedKey::Source { .. } => {
                    return Err(RuntimeError::BadKey {
                        key: entry.key.to_string(),
                        reason: "source-shaped key in layer_artifacts".into(),
                    });
                }
            }
        }
        for EmptyLayerCell { layer, cell } in &manifest.empty_layer_cells {
            let key = LayerCellKey {
                layer: layer.clone(),
                band: cell.band.clone(),
                x: cell.x,
                y: cell.y,
            };
            if layer_index.contains_key(&key) {
                return Err(RuntimeError::BadKey {
                    key: format!("{layer}/{band}/{x}_{y}", band = cell.band, x = cell.x, y = cell.y),
                    reason: "cell present in both layer_artifacts and empty_layer_cells".into(),
                });
            }
            layer_index.insert(key, LayerCellState::Empty);
        }

        let mut source_index = HashMap::with_capacity(manifest.source_artifacts.len());
        for entry in &manifest.source_artifacts {
            match parse(&entry.key)? {
                ParsedKey::Source { collection, cell } => {
                    let key = SourceCellKey {
                        collection: Arc::<str>::from(collection),
                        band: cell.band,
                        x: cell.x,
                        y: cell.y,
                    };
                    if source_index.contains_key(&key) {
                        return Err(RuntimeError::BadKey {
                            key: entry.key.to_string(),
                            reason: "duplicate (collection, band, cell) in source_artifacts".into(),
                        });
                    }
                    source_index.insert(key, entry.clone());
                }
                ParsedKey::Layer { .. } => {
                    return Err(RuntimeError::BadKey {
                        key: entry.key.to_string(),
                        reason: "layer-shaped key in source_artifacts".into(),
                    });
                }
            }
        }

        Ok(Self {
            canonical_crs,
            bands,
            layer_order,
            stylesheet,
            manifest,
            layer_index,
            source_index,
        })
    }
}

pub(crate) fn build_bands(config: &Config) -> Result<Vec<BandConfig>, RuntimeError> {
    let sizes = config.cells.size_per_band_m()?;
    let origin = (config.cells.origin[0], config.cells.origin[1]);
    let mut out = Vec::with_capacity(config.scales.bands.len());
    for b in &config.scales.bands {
        let cell_size = *sizes.get(&b.name).ok_or_else(|| {
            mars_config::ConfigError::Invalid(format!("no cells.size_per_band for band '{}'", b.name))
        })?;
        let max_denom = u32::try_from(b.max_denom)
            .map_err(|_| mars_config::ConfigError::Invalid(format!("band '{}' max_denom out of u32 range", b.name)))?;
        out.push(BandConfig {
            name: ScaleBand::new(b.name.clone()),
            max_denom,
            origin,
            cell_size,
        });
    }
    out.sort_by_key(|b| b.max_denom);
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use mars_config::{ArtifactCache, ArtifactStore, Artifacts, Band, Cells, Config, Scales, ServiceMeta, Source};
    use mars_types::{ArtifactEntry, ArtifactKey, Cell, ContentHash, Manifest, ScaleBand};

    use super::*;

    fn minimal_config() -> Config {
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
            scales: Scales {
                bands: vec![Band {
                    name: "hi".into(),
                    max_denom: 25000,
                }],
            },
            cells: Cells {
                grid: "regular".into(),
                origin: [0.0, 0.0],
                size_per_band,
                extent: None,
            },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![],
            observability: Default::default(),
            render: Default::default(),
            compiler: Default::default(),
        }
    }

    fn manifest_with_layer_key(key: &str) -> Manifest {
        Manifest::new(
            1,
            "t",
            vec![],
            vec![ArtifactEntry {
                key: ArtifactKey::new(key),
                hash: ContentHash::zero(),
                size_bytes: 0,
            }],
            None,
            vec![],
        )
    }

    fn manifest_with_source_key(key: &str) -> Manifest {
        Manifest::new(
            1,
            "t",
            vec![ArtifactEntry {
                key: ArtifactKey::new(key),
                hash: ContentHash::zero(),
                size_bytes: 0,
            }],
            vec![],
            None,
            vec![],
        )
    }

    #[test]
    fn rejects_source_key_in_layer_artifacts() {
        let cfg = minimal_config();
        let manifest = manifest_with_layer_key("src/coll/hi/0_0/abcd.mars");
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        assert!(matches!(err, RuntimeError::BadKey { .. }));
        let msg = err.to_string();
        assert!(msg.contains("source-shaped"), "error should say source-shaped: {msg}");
    }

    #[test]
    fn rejects_layer_key_in_source_artifacts() {
        let cfg = minimal_config();
        let manifest = manifest_with_source_key("lyr/l/hi/0_0/v1/abcd.mars");
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        assert!(matches!(err, RuntimeError::BadKey { .. }));
        let msg = err.to_string();
        assert!(msg.contains("layer-shaped"), "error should say layer-shaped: {msg}");
    }

    #[test]
    fn build_bands_sorts_by_max_denom() {
        let mut cfg = minimal_config();
        cfg.scales.bands = vec![
            Band {
                name: "lo".into(),
                max_denom: 100000,
            },
            Band {
                name: "hi".into(),
                max_denom: 25000,
            },
        ];
        cfg.cells.size_per_band.insert("lo".into(), "8192m".into());
        let bands = build_bands(&cfg).unwrap();
        assert_eq!(bands[0].name.as_str(), "hi");
        assert_eq!(bands[1].name.as_str(), "lo");
        assert_eq!(bands[0].max_denom, 25000);
        assert_eq!(bands[1].max_denom, 100000);
    }

    #[test]
    fn build_bands_errors_on_missing_size() {
        let mut cfg = minimal_config();
        cfg.scales.bands.push(Band {
            name: "ghost".into(),
            max_denom: 1000,
        });
        let err = build_bands(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "error should name missing band: {msg}");
    }

    #[test]
    fn rejects_duplicate_present_and_empty_marker() {
        let cfg = minimal_config();
        let key = "lyr/l/hi/0_0/v1/abcd.mars";
        let manifest = Manifest::new(
            1,
            "t",
            vec![],
            vec![ArtifactEntry {
                key: ArtifactKey::new(key),
                hash: ContentHash::zero(),
                size_bytes: 0,
            }],
            None,
            vec![EmptyLayerCell {
                layer: LayerId::new("l"),
                cell: Cell {
                    band: ScaleBand::new("hi"),
                    x: 0,
                    y: 0,
                },
            }],
        );
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        assert!(matches!(err, RuntimeError::BadKey { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("both layer_artifacts and empty_layer_cells"),
            "error: {msg}"
        );
    }

    #[test]
    fn rejects_duplicate_layer_artifact_cells() {
        let cfg = minimal_config();
        let key = "lyr/l/hi/0_0/v1/abcd.mars";
        let entry = ArtifactEntry {
            key: ArtifactKey::new(key),
            hash: ContentHash::zero(),
            size_bytes: 0,
        };
        let manifest = Manifest::new(1, "t", vec![], vec![entry.clone(), entry], None, vec![]);
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, RuntimeError::BadKey { .. }));
        assert!(msg.contains("duplicate"), "error: {msg}");
    }

    #[test]
    fn rejects_duplicate_source_artifact_cells() {
        let cfg = minimal_config();
        let key = "src/coll/hi/0_0/abcd.mars";
        let entry = ArtifactEntry {
            key: ArtifactKey::new(key),
            hash: ContentHash::zero(),
            size_bytes: 0,
        };
        let manifest = Manifest::new(1, "t", vec![entry.clone(), entry], vec![], None, vec![]);
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, RuntimeError::BadKey { .. }));
        assert!(msg.contains("duplicate"), "error: {msg}");
    }

    #[test]
    fn empty_marker_populates_index() {
        let cfg = minimal_config();
        let manifest = Manifest::new(
            1,
            "t",
            vec![],
            vec![],
            None,
            vec![EmptyLayerCell {
                layer: LayerId::new("l"),
                cell: Cell {
                    band: ScaleBand::new("hi"),
                    x: 0,
                    y: 0,
                },
            }],
        );
        let state = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap();
        assert_eq!(state.layer_index.len(), 1);
        assert!(matches!(
            state.layer_index.get(&LayerCellRef {
                layer: "l",
                band: "hi",
                x: 0,
                y: 0,
            }),
            Some(LayerCellState::Empty)
        ));
    }
}
