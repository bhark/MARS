//! manifest-key parser. SPEC §10.x key layout.
//!
//! layer artifact: `lyr/{layer}/{band}/{cx}_{cy}/v{schema_version}/{hash}.mars`
//! source artifact: `src/{collection}/{band}/{cx}_{cy}/{hash}.mars`

use mars_types::{ArtifactKey, Cell, LayerId, ScaleBand};

use crate::RuntimeError;

/// schema version baked into layer-artifact keys (FORMAT v1).
pub const LAYER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedKey {
    Layer { layer: LayerId, cell: Cell },
    Source { collection: String, cell: Cell },
}

pub(crate) fn parse(key: &ArtifactKey) -> Result<ParsedKey, RuntimeError> {
    let s = key.as_str();
    let parts: Vec<&str> = s.split('/').collect();
    let bad = || RuntimeError::BadKey(s.to_owned());
    match parts.as_slice() {
        // lyr/{layer}/{band}/{cx}_{cy}/v{schema}/{hash}.mars
        ["lyr", layer, band, cell, vseg, leaf] => {
            if !vseg.starts_with('v') || !leaf.ends_with(".mars") {
                return Err(bad());
            }
            let (cx, cy) = parse_cell_xy(cell).ok_or_else(bad)?;
            Ok(ParsedKey::Layer {
                layer: LayerId::new((*layer).to_owned()),
                cell: Cell {
                    band: ScaleBand::new((*band).to_owned()),
                    x: cx,
                    y: cy,
                },
            })
        }
        // src/{collection}/{band}/{cx}_{cy}/{hash}.mars
        ["src", coll, band, cell, leaf] => {
            if !leaf.ends_with(".mars") {
                return Err(bad());
            }
            let (cx, cy) = parse_cell_xy(cell).ok_or_else(bad)?;
            Ok(ParsedKey::Source {
                collection: (*coll).to_owned(),
                cell: Cell {
                    band: ScaleBand::new((*band).to_owned()),
                    x: cx,
                    y: cy,
                },
            })
        }
        _ => Err(bad()),
    }
}

fn parse_cell_xy(seg: &str) -> Option<(i64, i64)> {
    let (x, y) = seg.split_once('_')?;
    Some((x.parse().ok()?, y.parse().ok()?))
}

/// build a layer-artifact key. compiler and runtime agree on this format.
#[must_use]
pub fn layer_key(layer: &LayerId, cell: &Cell, hash_hex: &str) -> ArtifactKey {
    ArtifactKey::new(format!(
        "lyr/{layer}/{band}/{cx}_{cy}/v{ver}/{hash_hex}.mars",
        layer = layer.as_str(),
        band = cell.band.as_str(),
        cx = cell.x,
        cy = cell.y,
        ver = LAYER_SCHEMA_VERSION,
    ))
}

/// build a source-artifact key.
#[must_use]
pub fn source_key(collection: &str, cell: &Cell, hash_hex: &str) -> ArtifactKey {
    ArtifactKey::new(format!(
        "src/{collection}/{band}/{cx}_{cy}/{hash_hex}.mars",
        band = cell.band.as_str(),
        cx = cell.x,
        cy = cell.y,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_layer_and_source() {
        let lk = ArtifactKey::new("lyr/parcels/hi/3_-2/v1/abcd.mars");
        match parse(&lk).unwrap() {
            ParsedKey::Layer { layer, cell } => {
                assert_eq!(layer.as_str(), "parcels");
                assert_eq!(cell.band.as_str(), "hi");
                assert_eq!((cell.x, cell.y), (3, -2));
            }
            ParsedKey::Source { .. } => panic!("expected layer"),
        }
        let sk = ArtifactKey::new("src/buildings/hi/3_-2/abcd.mars");
        match parse(&sk).unwrap() {
            ParsedKey::Source { collection, cell } => {
                assert_eq!(collection, "buildings");
                assert_eq!((cell.x, cell.y), (3, -2));
            }
            ParsedKey::Layer { .. } => panic!("expected source"),
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse(&ArtifactKey::new("nope")).is_err());
        assert!(parse(&ArtifactKey::new("lyr/x/y/3_z/v1/a.mars")).is_err());
        assert!(parse(&ArtifactKey::new("lyr/x/y/3_4/x1/a.mars")).is_err());
    }
}
