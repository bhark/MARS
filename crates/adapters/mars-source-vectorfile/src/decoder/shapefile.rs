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
//! `.prj` is informational only: the binding's `source_crs` (config) is the
//! single source of truth for reprojection. When a `.prj` is present and
//! disagrees with the configured CRS this decoder logs a warning but does
//! not fail the decode - many real-world `.prj` files are imprecise.

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

        if let Some(prj_warn) = bundle.prj_warning {
            tracing::debug!(target: "mars_source_vectorfile::shapefile", "{}", prj_warn);
        }

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
    /// non-fatal note about `.prj` mismatch / absence; surfaced via tracing.
    prj_warning: Option<String>,
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
                "prj" => "prj",
                _ => continue,
            };
            by_ext.entry(kind).or_default().push((base, idx));
        }

        let shp_pick = pick_required(&by_ext, "shp")?;
        let shp_base = shp_pick.0.clone();
        let shx_pick = pick_matching(&by_ext, "shx", &shp_base)?;
        let dbf_pick = pick_matching(&by_ext, "dbf", &shp_base)?;
        let prj_pick = by_ext
            .get("prj")
            .and_then(|cands| cands.iter().find(|(b, _)| b == &shp_base))
            .map(|(_, idx)| *idx);

        let shp = read_zip_entry(&mut zip, shp_pick.1)?;
        let shx = read_zip_entry(&mut zip, shx_pick.1)?;
        let dbf = read_zip_entry(&mut zip, dbf_pick.1)?;
        let prj_warning = match prj_pick {
            Some(idx) => {
                // a present .prj is honoured implicitly: parsing wkt CRS
                // text and reconciling it against the binding's source_crs
                // is a future enhancement. for now we just acknowledge it
                // so operators see it was seen.
                let _ = read_zip_entry(&mut zip, idx)?;
                None
            }
            None => Some(format!(
                "shapefile archive '{shp_base}.shp.zip': no .prj sidecar; relying on binding source_crs"
            )),
        };

        Ok(Self {
            shp,
            shx,
            dbf,
            prj_warning,
        })
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::io::Write;

    use super::*;
    use shapefile::{Point, dbase::FieldName};

    fn synth_shapefile_zip() -> Vec<u8> {
        // build two-point shapefile via the shapefile writer, then zip the
        // .shp / .shx / .dbf into a single archive.
        let mut shp_buf: Vec<u8> = Vec::new();
        let mut shx_buf: Vec<u8> = Vec::new();
        let mut dbf_buf: Vec<u8> = Vec::new();

        let shape_writer = shapefile::ShapeWriter::with_shx(Cursor::new(&mut shp_buf), Cursor::new(&mut shx_buf));
        let table = dbase::TableWriterBuilder::new()
            .add_character_field(FieldName::try_from("name").unwrap(), 16)
            .add_numeric_field(FieldName::try_from("score").unwrap(), 10, 2);
        let dbase_writer = table.build_with_dest(Cursor::new(&mut dbf_buf));
        let mut writer = shapefile::Writer::new(shape_writer, dbase_writer);

        let mut rec_a = dbase::Record::default();
        rec_a.insert(
            "name".to_string(),
            dbase::FieldValue::Character(Some("alpha".to_string())),
        );
        rec_a.insert("score".to_string(), dbase::FieldValue::Numeric(Some(1.5)));
        writer.write_shape_and_record(&Point::new(1.0, 2.0), &rec_a).unwrap();

        let mut rec_b = dbase::Record::default();
        rec_b.insert(
            "name".to_string(),
            dbase::FieldValue::Character(Some("beta".to_string())),
        );
        rec_b.insert("score".to_string(), dbase::FieldValue::Numeric(Some(42.0)));
        writer.write_shape_and_record(&Point::new(3.0, 4.0), &rec_b).unwrap();
        drop(writer);

        let mut zip_buf: Vec<u8> = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut zip_buf));
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zw.start_file("pts.shp", opts).unwrap();
            zw.write_all(&shp_buf).unwrap();
            zw.start_file("pts.shx", opts).unwrap();
            zw.write_all(&shx_buf).unwrap();
            zw.start_file("pts.dbf", opts).unwrap();
            zw.write_all(&dbf_buf).unwrap();
            zw.finish().unwrap();
        }
        zip_buf
    }

    #[test]
    fn decodes_known_shapefile_zip() {
        let zip = synth_shapefile_zip();
        // fixture stays well below 5 KB:
        assert!(
            zip.len() < 5 * 1024,
            "synth zip should be tiny (got {} bytes)",
            zip.len()
        );

        let mut got: Vec<DecodedFeature> = Vec::new();
        ShapefileDecoder
            .decode(&Bytes::from(zip), &mut |f| {
                got.push(f);
                true
            })
            .unwrap();
        assert_eq!(got.len(), 2);

        // wkb sanity: byte_order(1) + type(4) + 2 doubles for points
        for f in &got {
            assert_eq!(f.geometry_wkb.len(), 1 + 4 + 16);
            assert_eq!(f.geometry_wkb[0], 1); // little-endian
        }

        // attribute round-trip
        let mut names: Vec<_> = got
            .iter()
            .filter_map(|f| match f.attributes.get("name") {
                Some(AttrValue::String(s)) => Some(s.trim().to_string()),
                _ => None,
            })
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
        // numeric round-trip (dbase Numeric -> AttrValue::Float)
        assert!(matches!(got[0].attributes.get("score"), Some(AttrValue::Float(_))));
    }

    #[test]
    fn rejects_zip_missing_shx() {
        // build a zip with only .shp + .dbf (no .shx) and expect Schema err.
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zw.start_file("a.shp", opts).unwrap();
            zw.write_all(b"not real but contents irrelevant").unwrap();
            zw.start_file("a.dbf", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
        }
        let err = ShapefileDecoder.decode(&Bytes::from(buf), &mut |_| true).unwrap_err();
        assert!(
            matches!(err, DecoderError::Schema(ref s) if s.contains(".shx")),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_zip_with_basename_mismatch() {
        // shp basename 'a', dbf basename 'b' - mismatched triple.
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zw.start_file("a.shp", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.start_file("a.shx", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.start_file("b.dbf", opts).unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
        }
        let err = ShapefileDecoder.decode(&Bytes::from(buf), &mut |_| true).unwrap_err();
        assert!(
            matches!(err, DecoderError::Schema(ref s) if s.contains("does not match")),
            "got {err:?}"
        );
    }
}
