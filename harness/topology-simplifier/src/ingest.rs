//! TSV + hex-EWKB polygon fixture loader.
//!
//! operator dump format (one feature per line, tab-separated):
//!   <feature_id>\t<hex_ewkb>\n
//!
//! produced by:
//!   \COPY (SELECT id, encode(ST_AsEWKB(geom), 'hex') FROM <table>) TO '<path>'
//!
//! anything other than Polygon / MultiPolygon is logged on stderr and skipped;
//! the spike is scoped to shared-boundary cases where seam preservation is
//! the question. malformed lines (bad id, bad hex, WKB error) increment
//! per-class counters and the line is skipped — partial fixtures should not
//! abort a multi-million-feature run.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use mars_artifact::{FeatureGeom, GeomKind, wkb_to_feature_geom};

#[derive(Debug, Default)]
pub struct IngestStats {
    pub lines_read: u64,
    pub kept: u64,
    pub skipped_non_polygon: u64,
    pub skipped_bad_line: u64,
    pub skipped_bad_hex: u64,
    pub skipped_bad_wkb: u64,
}

pub fn load_fixture(path: &Path) -> anyhow::Result<(Vec<FeatureGeom>, IngestStats)> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(1 << 20, file);
    let mut out = Vec::new();
    let mut stats = IngestStats::default();

    for line in reader.lines() {
        let line = line?;
        stats.lines_read += 1;
        if line.is_empty() {
            continue;
        }
        let Some((id_str, hex_str)) = line.split_once('\t') else {
            stats.skipped_bad_line += 1;
            continue;
        };
        let Ok(id) = id_str.parse::<u64>() else {
            stats.skipped_bad_line += 1;
            continue;
        };
        // strip a postgres-style \\x prefix if present; standard ST_AsEWKB(.., 'hex')
        // does not emit one but defensive against hand-edited dumps.
        let hex_clean = hex_str.strip_prefix("\\x").unwrap_or(hex_str);
        let bytes = match hex::decode(hex_clean) {
            Ok(b) => b,
            Err(_) => {
                stats.skipped_bad_hex += 1;
                continue;
            }
        };
        let geom = match wkb_to_feature_geom(&bytes, id) {
            Ok(g) => g,
            Err(_) => {
                stats.skipped_bad_wkb += 1;
                continue;
            }
        };
        match geom.geom {
            GeomKind::Polygon(_) | GeomKind::MultiPolygon(_) => {
                out.push(geom);
                stats.kept += 1;
            }
            _ => {
                stats.skipped_non_polygon += 1;
            }
        }
    }
    Ok((out, stats))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;

    /// build a minimal little-endian WKB polygon (single ring, square).
    fn wkb_square_le(min: f64, max: f64) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(1u8); // little endian
        b.extend_from_slice(&3u32.to_le_bytes()); // POLYGON
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
        b.extend_from_slice(&5u32.to_le_bytes()); // 5 points (closed)
        let pts = [(min, min), (max, min), (max, max), (min, max), (min, min)];
        for (x, y) in pts {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&y.to_le_bytes());
        }
        b
    }

    fn wkb_point_le(x: f64, y: f64) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(1u8);
        b.extend_from_slice(&1u32.to_le_bytes()); // POINT
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b
    }

    #[test]
    fn ingest_filters_to_polygons_and_counts_skips() {
        let dir = tempdir_in_target("filters");
        let path = dir.join("dump.tsv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "1\t{}", hex::encode(wkb_square_le(0.0, 10.0))).unwrap();
        writeln!(f, "2\t{}", hex::encode(wkb_point_le(1.0, 2.0))).unwrap();
        writeln!(f, "3\tnot-hex").unwrap();
        writeln!(f, "no-tab-here").unwrap();
        writeln!(f, "5\t{}", hex::encode(wkb_square_le(20.0, 30.0))).unwrap();
        drop(f);

        let (geoms, stats) = load_fixture(&path).unwrap();
        assert_eq!(geoms.len(), 2);
        assert_eq!(stats.kept, 2);
        assert_eq!(stats.skipped_non_polygon, 1);
        assert_eq!(stats.skipped_bad_hex, 1);
        assert_eq!(stats.skipped_bad_line, 1);
        assert_eq!(geoms[0].user_id, 1);
        assert_eq!(geoms[1].user_id, 5);
    }

    #[test]
    fn ingest_strips_pg_hex_prefix() {
        let dir = tempdir_in_target("pg_prefix");
        let path = dir.join("dump.tsv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "9\t\\x{}", hex::encode(wkb_square_le(0.0, 1.0))).unwrap();
        drop(f);
        let (geoms, stats) = load_fixture(&path).unwrap();
        assert_eq!(geoms.len(), 1);
        assert_eq!(stats.kept, 1);
        assert_eq!(geoms[0].user_id, 9);
    }

    fn tempdir_in_target(tag: &str) -> std::path::PathBuf {
        // standalone workspace can't pull tempfile from MARS workspace.deps;
        // a CARGO_TARGET_TMPDIR-rooted dir is enough for unit tests. tag
        // disambiguates parallel tests sharing the same pid.
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("topo-ingest-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
