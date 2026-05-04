//! re-exports of the canonical [`ArtifactKey`] constructors and parser. the
//! key format itself is owned by `mars-types` so compiler and runtime cannot
//! drift apart on layout.

use mars_types::{ArtifactKey, Cell, ContentHash, LayerId, ParsedArtifactKey};

use crate::RuntimeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedKey {
    Layer { layer: LayerId, cell: Cell },
    Source { collection: String, cell: Cell },
}

pub(crate) fn parse(key: &ArtifactKey) -> Result<ParsedKey, RuntimeError> {
    match key.parse().map_err(|e| RuntimeError::BadKey {
        key: key.to_string(),
        reason: e.to_string(),
    })? {
        ParsedArtifactKey::Layer { layer, cell } => Ok(ParsedKey::Layer { layer, cell }),
        ParsedArtifactKey::Source { collection, cell } => Ok(ParsedKey::Source { collection, cell }),
    }
}

/// thin wrapper around [`ArtifactKey::build_layer`] for tests that pass a hex string.
#[must_use]
pub fn layer_key(layer: &LayerId, cell: &Cell, hash_hex: &str) -> ArtifactKey {
    ArtifactKey::build_layer(layer, cell, hash_from_hex(hash_hex))
}

/// thin wrapper around [`ArtifactKey::build_source`] for tests that pass a hex string.
#[must_use]
pub fn source_key(collection: &str, cell: &Cell, hash_hex: &str) -> ArtifactKey {
    ArtifactKey::build_source(collection, cell, hash_from_hex(hash_hex))
}

// fixture helper: parse 0..=64 hex chars into a 32-byte hash, zero-padded.
// not exposed: tests are the only call site and pass either 4 chars ("abcd")
// or a full 64-char digest.
fn hash_from_hex(hex: &str) -> ContentHash {
    let mut out = [0u8; 32];
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() && i / 2 < 32 {
        let hi = hex_nibble(bytes[i]).unwrap_or(0);
        let lo = hex_nibble(bytes[i + 1]).unwrap_or(0);
        out[i / 2] = (hi << 4) | lo;
        i += 2;
    }
    ContentHash(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_types::ScaleBand;

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

    #[test]
    fn builders_round_trip() {
        let layer = LayerId::new("parcels");
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };
        let lk = layer_key(&layer, &cell, "abcd");
        assert!(matches!(parse(&lk).unwrap(), ParsedKey::Layer { .. }));
        let sk = source_key("c", &cell, "abcd");
        assert!(matches!(parse(&sk).unwrap(), ParsedKey::Source { .. }));
    }
}
