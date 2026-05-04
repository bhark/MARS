//! immutable per-snapshot runtime state. built once per manifest version.

use std::collections::HashMap;

use mars_config::Config;
use mars_grid::BandConfig;
use mars_style::Stylesheet;
use mars_types::{ArtifactEntry, CrsCode, LayerId, Manifest, ScaleBand};

use crate::RuntimeError;
use crate::key::{ParsedKey, parse};

/// composite indexing key for layer artifacts.
pub type LayerCellKey = (LayerId, ScaleBand, (i64, i64));
/// composite indexing key for source artifacts (collection name kept as String;
/// `SourceCollectionId` is a port-side strong wrapper around the same string).
pub type SourceCellKey = (String, ScaleBand, (i64, i64));

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
    /// `(layer, band, cell) -> artifact entry`.
    pub layer_index: HashMap<LayerCellKey, ArtifactEntry>,
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

        let mut layer_index = HashMap::with_capacity(manifest.layer_artifacts.len());
        for entry in &manifest.layer_artifacts {
            match parse(&entry.key)? {
                ParsedKey::Layer { layer, cell } => {
                    layer_index.insert((layer, cell.band, (cell.x, cell.y)), entry.clone());
                }
                ParsedKey::Source { .. } => {
                    return Err(RuntimeError::BadKey(format!(
                        "source-shaped key in layer_artifacts: {}",
                        entry.key
                    )));
                }
            }
        }

        let mut source_index = HashMap::with_capacity(manifest.source_artifacts.len());
        for entry in &manifest.source_artifacts {
            match parse(&entry.key)? {
                ParsedKey::Source { collection, cell } => {
                    source_index.insert((collection, cell.band, (cell.x, cell.y)), entry.clone());
                }
                ParsedKey::Layer { .. } => {
                    return Err(RuntimeError::BadKey(format!(
                        "layer-shaped key in source_artifacts: {}",
                        entry.key
                    )));
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
    let sizes = config
        .cells
        .size_per_band_m()
        .map_err(|e| RuntimeError::Config(format!("cells.size_per_band: {e}")))?;
    let origin = (config.cells.origin[0], config.cells.origin[1]);
    let mut out = Vec::with_capacity(config.scales.bands.len());
    for b in &config.scales.bands {
        let cell_size = *sizes
            .get(&b.name)
            .ok_or_else(|| RuntimeError::Config(format!("no cells.size_per_band for band '{}'", b.name)))?;
        let max_denom = u32::try_from(b.max_denom)
            .map_err(|_| RuntimeError::Config(format!("band '{}' max_denom out of u32 range", b.name)))?;
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
    use mars_types::{ArtifactEntry, ArtifactKey, ContentHash, Manifest};

    use super::*;

    fn minimal_config() -> Config {
        let mut size_per_band = BTreeMap::new();
        size_per_band.insert("hi".into(), "4096m".into());
        Config {
            service: ServiceMeta { name: "t".into(), ..Default::default() },
            source: Source { kind: "memory".into(), dsn: "memory://".into(), native_crs: CrsCode::new("EPSG:25832"), change_feed: None },
            artifacts: Artifacts {
                store: ArtifactStore { kind: "fs".into(), endpoint: None, bucket: None, prefix: None, path: Some("/tmp".into()) },
                cache: ArtifactCache { path: "/tmp".into(), max_size: "1GiB".into(), eviction: "lru".into() },
            },
            scales: Scales { bands: vec![Band { name: "hi".into(), max_denom: 25000 }] },
            cells: Cells { grid: "regular".into(), origin: [0.0, 0.0], size_per_band, extent: None },
            interfaces: Default::default(),
            tile_matrix_sets: Default::default(),
            reprojection: Default::default(),
            styles: Default::default(),
            layers: vec![],
            observability: Default::default(),
        }
    }

    fn manifest_with_layer_key(key: &str) -> Manifest {
        Manifest {
            version: 1,
            service: "t".into(),
            source_artifacts: vec![],
            layer_artifacts: vec![ArtifactEntry {
                key: ArtifactKey::new(key),
                hash: ContentHash::zero(),
                size_bytes: 0,
            }],
            style_artifact: None,
        }
    }

    fn manifest_with_source_key(key: &str) -> Manifest {
        Manifest {
            version: 1,
            service: "t".into(),
            source_artifacts: vec![ArtifactEntry {
                key: ArtifactKey::new(key),
                hash: ContentHash::zero(),
                size_bytes: 0,
            }],
            layer_artifacts: vec![],
            style_artifact: None,
        }
    }

    #[test]
    fn rejects_source_key_in_layer_artifacts() {
        let cfg = minimal_config();
        let manifest = manifest_with_layer_key("src/coll/hi/0_0/abcd.mars");
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        assert!(matches!(err, RuntimeError::BadKey(_)));
        let msg = err.to_string();
        assert!(msg.contains("source-shaped"), "error should say source-shaped: {msg}");
    }

    #[test]
    fn rejects_layer_key_in_source_artifacts() {
        let cfg = minimal_config();
        let manifest = manifest_with_source_key("lyr/l/hi/0_0/v1/abcd.mars");
        let err = RuntimeState::from_config_and_manifest(&cfg, Stylesheet::default(), manifest).unwrap_err();
        assert!(matches!(err, RuntimeError::BadKey(_)));
        let msg = err.to_string();
        assert!(msg.contains("layer-shaped"), "error should say layer-shaped: {msg}");
    }

    #[test]
    fn build_bands_sorts_by_max_denom() {
        let mut cfg = minimal_config();
        cfg.scales.bands = vec![
            Band { name: "lo".into(), max_denom: 100000 },
            Band { name: "hi".into(), max_denom: 25000 },
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
        cfg.scales.bands.push(Band { name: "ghost".into(), max_denom: 1000 });
        let err = build_bands(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "error should name missing band: {msg}");
    }
}
