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

fn build_bands(config: &Config) -> Result<Vec<BandConfig>, RuntimeError> {
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
