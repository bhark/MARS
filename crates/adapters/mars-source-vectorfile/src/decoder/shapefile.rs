//! ESRI Shapefile decoder. Reads the `.shp` + `.shx` + `.dbf` triple from a
//! single ZIP archive (the binding's URI). Geometry is converted
//! `shapefile::Shape -> geo_types::Geometry -> OGC WKB` via geozero's
//! `ToWkb`; attributes come from the bundled DBase reader.
//!
//! Why a ZIP carrier: the public-facing decoder contract is `&Bytes -> ...`,
//! which is a single payload. Shapefile is multi-file by definition, and
//! the convention "one URI per binding, archive holds the sidecars" is the
//! standard way every public dataset ships shapefile (e.g. census tiger,
//! eurostat, OS open data). It keeps the fetcher and cache untouched.
//!
//! `.prj` sidecars are not parsed. The binding's `source_crs` (config) is
//! the single source of truth for the archive's native CRS. WKT-1 in
//! shipped `.prj` files is too inconsistent in practice to reconcile
//! against an EPSG code reliably, so operators are responsible for
//! ensuring `source_crs` matches the data.
//!
//! Raw `.shp` URIs (or `#format=shapefile` over a non-archive URI) are
//! rejected at fragment-resolution time with `FragmentError::Unsupported-
//! RawShapefile`, not at decode time: the `&Bytes` contract has no room
//! for sidecar fetching, and the upstream `shapefile::ShapeReader::with_shx`
//! + `dbase::Reader` together require all three files.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use bytes::Bytes;
use geozero::{CoordDimensions, ToWkb};
use mars_config::VectorFileFormat;
use mars_source::AttrValue;
use shapefile::dbase;

use super::{DecodedFeature, Decoder};
use crate::error::DecoderError;

/// Shapefile decoder. Bytes (zip archive) -> per-feature `DecodedFeature`.
pub struct ShapefileDecoder;

impl Decoder for ShapefileDecoder {
    fn name(&self) -> &'static str {
        "shapefile"
    }

    fn supports(&self, format: VectorFileFormat) -> bool {
        matches!(format, VectorFileFormat::Shapefile)
    }

    fn decode(&self, bytes: &Bytes, sink: &mut dyn FnMut(DecodedFeature) -> bool) -> Result<(), DecoderError> {
        // policy notice: once per process, debug-level. operators investigating
        // crs questions see it; normal logs stay quiet.
        static PRJ_POLICY_LOGGED: std::sync::Once = std::sync::Once::new();
        PRJ_POLICY_LOGGED.call_once(|| {
            tracing::debug!(
                target: "mars_source_vectorfile::shapefile",
                "shapefile decoder: .prj sidecars are not parsed; binding source_crs is authoritative"
            );
        });

        let bundle = ShapefileBundle::from_zip(bytes)?;
        let shp = Cursor::new(bundle.shp);
        let shx = Cursor::new(bundle.shx);
        let dbf_cur = Cursor::new(bundle.dbf);

        // shx is required for the typed reader; passing both yields the
        // indexed iterator the shapefile crate exposes.
        let shape_reader = shapefile::ShapeReader::with_shx(shp, shx).map_err(parse_err)?;
        let dbase_reader = dbase::Reader::new(dbf_cur).map_err(|e| DecoderError::Parse {
            format: "shapefile",
            source: Box::new(e),
        })?;
        let mut reader = shapefile::Reader::new(shape_reader, dbase_reader);

        for rec in reader.iter_shapes_and_records() {
            let (shape, record) = rec.map_err(parse_err)?;

            // null geometries are dropped to match the geojson decoder and
            // the postgis adapter's null-geom behaviour.
            if matches!(shape, shapefile::Shape::NullShape) {
                continue;
            }

            let geometry: geo_types::Geometry<f64> = match shape.try_into() {
                Ok(g) => g,
                Err(e) => {
                    return Err(DecoderError::Schema(format!(
                        "shapefile shape -> geo_types conversion failed: {e}"
                    )));
                }
            };
            let wkb = geometry
                .to_wkb(CoordDimensions::xy())
                .map_err(|e| DecoderError::Parse {
                    format: "shapefile",
                    source: Box::new(e),
                })?;

            let attributes = record_to_attrs(&record);

            // shapefile has no native fid - use 0 so the stream layer
            // assigns the row index (mirrors the flatgeobuf decoder).
            let cont = sink(DecodedFeature {
                feature_id: 0,
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

struct ShapefileBundle {
    shp: Vec<u8>,
    shx: Vec<u8>,
    dbf: Vec<u8>,
}

impl ShapefileBundle {
    fn from_zip(bytes: &Bytes) -> Result<Self, DecoderError> {
        let cursor = Cursor::new(bytes.clone());
        let mut zip = zip::ZipArchive::new(cursor).map_err(|e| DecoderError::Parse {
            format: "shapefile",
            source: Box::new(e),
        })?;

        // collect candidates by lowercase extension; keep basename so we can
        // pick the matching triple when an archive contains multiple datasets.
        let mut by_ext: HashMap<&'static str, Vec<(String, usize)>> = HashMap::new();
        for idx in 0..zip.len() {
            let entry = zip.by_index(idx).map_err(|e| DecoderError::Parse {
                format: "shapefile",
                source: Box::new(e),
            })?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            let (base, ext) = match split_basename_ext(&name) {
                Some(v) => v,
                None => continue,
            };
            let kind = match ext.as_str() {
                "shp" => "shp",
                "shx" => "shx",
                "dbf" => "dbf",
                _ => continue,
            };
            by_ext.entry(kind).or_default().push((base, idx));
        }

        let shp_pick = pick_required(&by_ext, "shp")?;
        let shp_base = shp_pick.0.clone();
        let shx_pick = pick_matching(&by_ext, "shx", &shp_base)?;
        let dbf_pick = pick_matching(&by_ext, "dbf", &shp_base)?;

        let shp = read_zip_entry(&mut zip, shp_pick.1)?;
        let shx = read_zip_entry(&mut zip, shx_pick.1)?;
        let dbf = read_zip_entry(&mut zip, dbf_pick.1)?;

        Ok(Self { shp, shx, dbf })
    }
}

fn pick_required<'a>(
    by_ext: &'a HashMap<&'static str, Vec<(String, usize)>>,
    ext: &'static str,
) -> Result<&'a (String, usize), DecoderError> {
    let cands = by_ext
        .get(ext)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| DecoderError::Schema(format!("shapefile zip missing required .{ext} sidecar")))?;
    if cands.len() > 1 {
        // ambiguous archive: an empty/unknown-basename one keeps the convention.
        // we still pick the first - real archives ship a single dataset.
        tracing::warn!(
            target: "mars_source_vectorfile::shapefile",
            "shapefile zip contains multiple .{ext} entries; picking first: {}",
            cands[0].0
        );
    }
    Ok(&cands[0])
}

fn pick_matching<'a>(
    by_ext: &'a HashMap<&'static str, Vec<(String, usize)>>,
    ext: &'static str,
    base: &str,
) -> Result<&'a (String, usize), DecoderError> {
    let cands = by_ext.get(ext).filter(|v| !v.is_empty()).ok_or_else(|| {
        DecoderError::Schema(format!(
            "shapefile zip missing required .{ext} sidecar for basename '{base}'"
        ))
    })?;
    cands.iter().find(|(b, _)| b == base).ok_or_else(|| {
        DecoderError::Schema(format!(
            "shapefile zip has .{ext} but its basename does not match the .shp ('{base}')"
        ))
    })
}

fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    idx: usize,
) -> Result<Vec<u8>, DecoderError> {
    let mut entry = zip.by_index(idx).map_err(|e| DecoderError::Parse {
        format: "shapefile",
        source: Box::new(e),
    })?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).map_err(|e| DecoderError::Parse {
        format: "shapefile",
        source: Box::new(e),
    })?;
    Ok(buf)
}

/// Split `path/to/foo.shp` into ("foo", "shp"). Returns None when the entry
/// has no extension or its basename is empty. Strips any leading directory
/// so nested archives still resolve sibling sidecars.
fn split_basename_ext(name: &str) -> Option<(String, String)> {
    let tail = match name.rsplit_once('/') {
        Some((_, t)) => t,
        None => name,
    };
    let (stem, ext) = tail.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some((stem.to_string(), ext.to_ascii_lowercase()))
}

fn record_to_attrs(record: &dbase::Record) -> HashMap<String, AttrValue> {
    let inner: &HashMap<String, dbase::FieldValue> = record.as_ref();
    let mut out = HashMap::with_capacity(inner.len());
    for (name, value) in inner {
        out.insert(name.clone(), dbase_to_attr(value));
    }
    out
}

fn dbase_to_attr(v: &dbase::FieldValue) -> AttrValue {
    match v {
        dbase::FieldValue::Character(Some(s)) => AttrValue::String(s.clone()),
        dbase::FieldValue::Numeric(Some(n)) => AttrValue::Float(*n),
        dbase::FieldValue::Logical(Some(b)) => AttrValue::Bool(*b),
        dbase::FieldValue::Date(Some(d)) => AttrValue::String(format!("{}-{:02}-{:02}", d.year(), d.month(), d.day())),
        dbase::FieldValue::Float(Some(f)) => AttrValue::Float(f64::from(*f)),
        dbase::FieldValue::Integer(i) => AttrValue::Int(i64::from(*i)),
        dbase::FieldValue::Double(d) => AttrValue::Float(*d),
        dbase::FieldValue::DateTime(_) | dbase::FieldValue::Memo(_) | dbase::FieldValue::Currency(_) => {
            // serialise non-primitive variants via Display so the downstream
            // expr layer can still match strings; matches the geojson
            // decoder's fallback for nested/unrepresentable values.
            AttrValue::String(format!("{v}"))
        }
        // explicit-None variants fall through as null
        _ => AttrValue::Null,
    }
}

fn parse_err(e: shapefile::Error) -> DecoderError {
    DecoderError::Parse {
        format: "shapefile",
        source: Box::new(e),
    }
}

#[cfg(test)]
mod tests;
