//! OGC GeoPackage decoder. A GeoPackage is a SQLite database (`.gpkg`) with
//! a documented schema for feature tables; this decoder writes the fetched
//! bytes to a tempfile so SQLite can `mmap` it, then iterates the configured
//! feature table emitting OGC WKB + attribute rows.
//!
//! Feature-table discovery follows the GeoPackage 1.4 spec: rows in
//! `gpkg_contents` with `data_type = 'features'` enumerate the feature
//! tables; each entry's matching row in `gpkg_geometry_columns` names the
//! geometry column. v1 picks the first feature table when a single binding
//! URI carries multiple - mirroring the shapefile decoder's behaviour for
//! multi-dataset archives. Operators with multi-table containers point one
//! binding URI per table.
//!
//! Geometry blobs follow the GeoPackageBinary header (§3.1.2 / 4.1.2):
//! `GP` magic + version + flags + srs_id + optional envelope + OGC WKB. The
//! envelope is informational; we skip it and emit the trailing WKB. ExtendedGeoPackageBinary
//! (flags bit 5 set) is rejected with a typed error - extensions are
//! out of scope.

use std::collections::HashMap;
use std::io::Write;

use bytes::Bytes;
use mars_config::VectorFileFormat;
use mars_source::AttrValue;
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use super::{DecodedFeature, Decoder};
use crate::error::DecoderError;

/// GeoPackage decoder. Bytes (.gpkg blob) -> per-feature `DecodedFeature`.
pub struct GeoPackageDecoder;

impl Decoder for GeoPackageDecoder {
    fn name(&self) -> &'static str {
        "geopackage"
    }

    fn supports(&self, format: VectorFileFormat) -> bool {
        matches!(format, VectorFileFormat::GeoPackage)
    }

    fn decode(&self, bytes: &Bytes, sink: &mut dyn FnMut(DecodedFeature) -> bool) -> Result<(), DecoderError> {
        // sqlite opens by path, not by buffer; spool to a tempfile so the
        // open + mmap path stays stock and we don't pay for an in-memory
        // ATTACH dance.
        let mut tmp = tempfile::NamedTempFile::new().map_err(io_err)?;
        tmp.write_all(bytes).map_err(io_err)?;
        tmp.flush().map_err(io_err)?;

        let conn =
            Connection::open_with_flags(tmp.path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(parse_err)?;

        let table = first_feature_table(&conn)?;
        let geom_col = geometry_column_for(&conn, &table)?;
        let columns = table_columns(&conn, &table)?;
        let pk_col = primary_key_column(&conn, &table)?;
        let attr_cols: Vec<&str> = columns
            .iter()
            .map(String::as_str)
            .filter(|c| *c != geom_col.as_str() && Some(*c) != pk_col.as_deref())
            .collect();

        // We list the geometry first, then pk (or NULL when absent), then the
        // remaining attribute columns. The decoder is conservative about
        // identifier quoting - feature/attribute column names are validated
        // against a strict character set above to keep SQL injection out of
        // reach.
        let pk_select = pk_col
            .as_deref()
            .map(|c| format!(r#""{c}""#))
            .unwrap_or_else(|| "NULL".to_string());
        let attr_select = attr_cols
            .iter()
            .map(|c| format!(r#""{c}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = if attr_cols.is_empty() {
            format!(r#"SELECT "{geom_col}", {pk_select} FROM "{table}""#)
        } else {
            format!(r#"SELECT "{geom_col}", {pk_select}, {attr_select} FROM "{table}""#)
        };

        let mut stmt = conn.prepare(&sql).map_err(parse_err)?;
        let mut rows = stmt.query([]).map_err(parse_err)?;
        while let Some(row) = rows.next().map_err(parse_err)? {
            // null geometry: drop the row (mirrors geojson / shapefile / postgis null-geom).
            let blob: Option<Vec<u8>> = row.get(0).map_err(parse_err)?;
            let Some(blob) = blob else {
                continue;
            };
            let wkb = strip_gpkg_header(&blob)?;

            let feature_id = match row.get_ref(1).map_err(parse_err)? {
                ValueRef::Null => 0,
                ValueRef::Integer(i) => u64::try_from(i).unwrap_or(0),
                _ => 0,
            };

            let mut attributes: HashMap<String, AttrValue> = HashMap::with_capacity(attr_cols.len());
            for (idx, name) in attr_cols.iter().enumerate() {
                // pk lives at index 1; attribute columns start at 2.
                let v = row.get_ref(2 + idx).map_err(parse_err)?;
                attributes.insert((*name).to_string(), value_ref_to_attr(v));
            }

            let cont = sink(DecodedFeature {
                feature_id,
                geometry_wkb: Bytes::from(wkb),
                attributes,
            });
            if !cont {
                break;
            }
        }
        Ok(())
    }
}

/// Walk `gpkg_contents` for the first row with `data_type = 'features'`.
/// Returns the table name; errors when no feature table is registered.
fn first_feature_table(conn: &Connection) -> Result<String, DecoderError> {
    let mut stmt = conn
        .prepare("SELECT table_name FROM gpkg_contents WHERE data_type = 'features' ORDER BY table_name LIMIT 1")
        .map_err(|e| schema_err(&format!("gpkg_contents missing or unreadable: {e}")))?;
    let mut rows = stmt.query([]).map_err(parse_err)?;
    let Some(row) = rows.next().map_err(parse_err)? else {
        return Err(schema_err(
            "gpkg has no feature table (gpkg_contents.data_type = 'features')",
        ));
    };
    let name: String = row.get(0).map_err(parse_err)?;
    validate_identifier(&name)?;
    Ok(name)
}

/// Look up the geometry column for `table` via `gpkg_geometry_columns`.
fn geometry_column_for(conn: &Connection, table: &str) -> Result<String, DecoderError> {
    let mut stmt = conn
        .prepare("SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ? LIMIT 1")
        .map_err(parse_err)?;
    let mut rows = stmt.query([table]).map_err(parse_err)?;
    let Some(row) = rows.next().map_err(parse_err)? else {
        return Err(schema_err(&format!(
            "gpkg feature table {table:?} has no entry in gpkg_geometry_columns"
        )));
    };
    let name: String = row.get(0).map_err(parse_err)?;
    validate_identifier(&name)?;
    Ok(name)
}

/// List every column on `table` via the PRAGMA table_info virtual table.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, DecoderError> {
    let sql = format!(r#"PRAGMA table_info("{table}")"#);
    let mut stmt = conn.prepare(&sql).map_err(parse_err)?;
    let mut rows = stmt.query([]).map_err(parse_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(parse_err)? {
        // table_info columns: cid, name, type, notnull, dflt_value, pk
        let name: String = row.get(1).map_err(parse_err)?;
        validate_identifier(&name)?;
        out.push(name);
    }
    Ok(out)
}

/// Pick the INTEGER PRIMARY KEY column (if any) for `table`. GeoPackage
/// requires a single-column INTEGER pk on every feature table; we still
/// tolerate its absence to stay forgiving on hand-rolled files.
fn primary_key_column(conn: &Connection, table: &str) -> Result<Option<String>, DecoderError> {
    let sql = format!(r#"PRAGMA table_info("{table}")"#);
    let mut stmt = conn.prepare(&sql).map_err(parse_err)?;
    let mut rows = stmt.query([]).map_err(parse_err)?;
    while let Some(row) = rows.next().map_err(parse_err)? {
        let name: String = row.get(1).map_err(parse_err)?;
        let pk: i64 = row.get(5).map_err(parse_err)?;
        if pk == 1 {
            validate_identifier(&name)?;
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// Strip the GeoPackageBinary header per GPKG 1.4 §4.1.2 and return the
/// trailing OGC WKB. The header layout is:
///   bytes 0..1  : magic "GP"
///   byte  2     : version (always 0 for spec-compliant containers)
///   byte  3     : flags (envelope contents indicator + endianness + extended)
///   bytes 4..7  : srs_id (i32, native endianness per flags)
///   bytes 8..N  : optional envelope (size derived from flags)
///   bytes N..   : OGC WKB
fn strip_gpkg_header(blob: &[u8]) -> Result<Vec<u8>, DecoderError> {
    if blob.len() < 8 {
        return Err(schema_err("gpkg geometry blob shorter than header"));
    }
    if &blob[..2] != b"GP" {
        return Err(schema_err("gpkg geometry blob missing 'GP' magic"));
    }
    // version byte at [2]; not interesting for parsing
    let flags = blob[3];
    // bit 5: extended GeoPackageBinary type. unsupported for now.
    if flags & 0b0010_0000 != 0 {
        return Err(schema_err("extended GeoPackageBinary geometry blobs are not supported"));
    }
    // bits 1..3 carry the envelope indicator (0..4); other values reserved.
    let envelope_indicator = (flags >> 1) & 0b0000_0111;
    let envelope_bytes = match envelope_indicator {
        0 => 0,
        1 => 32, // xy (4 * 8)
        2 => 48, // xyz or xym (6 * 8)
        3 => 48,
        4 => 64, // xyzm
        _ => {
            return Err(schema_err(
                "reserved envelope indicator value in GeoPackageBinary header",
            ));
        }
    };
    let header_len = 8 + envelope_bytes;
    if blob.len() < header_len {
        return Err(schema_err("gpkg blob shorter than declared envelope"));
    }
    Ok(blob[header_len..].to_vec())
}

fn value_ref_to_attr(v: ValueRef<'_>) -> AttrValue {
    match v {
        ValueRef::Null => AttrValue::Null,
        ValueRef::Integer(i) => AttrValue::Int(i),
        ValueRef::Real(f) => AttrValue::Float(f),
        ValueRef::Text(t) => AttrValue::String(String::from_utf8_lossy(t).into_owned()),
        // raw blobs lose fidelity - downstream string matching is the best we
        // can do without expanding the AttrValue surface.
        ValueRef::Blob(_) => AttrValue::Null,
    }
}

/// Reject identifiers that could be SQL-injected through the dynamic
/// `SELECT ... FROM "<table>"`. Only ascii alphanumerics, `_`, and `-` are
/// accepted - GeoPackage table / column names in real-world files stay
/// within this range.
fn validate_identifier(name: &str) -> Result<(), DecoderError> {
    if name.is_empty() {
        return Err(schema_err("gpkg returned empty identifier"));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(schema_err(&format!(
                "gpkg identifier {name:?} contains an unsupported character (allowed: alphanumeric, _, -)"
            )));
        }
    }
    Ok(())
}

fn parse_err(e: rusqlite::Error) -> DecoderError {
    DecoderError::Parse {
        format: "geopackage",
        source: Box::new(e),
    }
}

fn schema_err(msg: &str) -> DecoderError {
    DecoderError::Schema(msg.to_string())
}

fn io_err(e: std::io::Error) -> DecoderError {
    DecoderError::Parse {
        format: "geopackage",
        source: Box::new(e),
    }
}

#[cfg(test)]
mod tests;
