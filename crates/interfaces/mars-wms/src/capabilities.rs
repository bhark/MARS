//! WMS 1.3.0 GetCapabilities document. SPEC §12.
//!
//! We render a minimal, valid 1.3.0 capabilities body; format conformance is a
//! Phase 2 concern. The output is built per manifest swap so newly published
//! layer bboxes show up without restarting.

use std::collections::HashMap;
use std::io::Cursor;

use mars_config::Config;
use mars_types::{Bbox, ImageFormat, LayerId, Manifest, ParsedArtifactKey};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::WmsError;

/// Render the capabilities XML. Per-layer bboxes are taken from the union of
/// materialised artifact cells in the manifest, falling back to the layer's
/// configured `bbox` when no artifacts exist yet. Per-layer scale ranges are
/// derived from the layer scale window. The root `BoundingBox` is the union
/// of layer bboxes; the element is omitted if neither source produces one.
pub fn capabilities_xml(cfg: &Config, manifest: &Manifest) -> Result<String, WmsError> {
    let layer_bboxes = derive_layer_bboxes(cfg, manifest);
    let root_bbox = layer_bboxes.values().copied().reduce(union_bbox);

    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    let mut root = BytesStart::new("WMS_Capabilities");
    root.push_attribute(("version", "1.3.0"));
    root.push_attribute(("xmlns", "http://www.opengis.net/wms"));
    w.write_event(Event::Start(root)).map_err(xml_err)?;

    // service block
    w.write_event(Event::Start(BytesStart::new("Service")))
        .map_err(xml_err)?;
    text_element(&mut w, "Name", "WMS")?;
    text_element(&mut w, "Title", &cfg.service.title)?;
    text_element(&mut w, "Abstract", &cfg.service.abstract_)?;
    if !cfg.service.contact_email.is_empty() {
        w.write_event(Event::Start(BytesStart::new("ContactInformation")))
            .map_err(xml_err)?;
        text_element(&mut w, "ContactElectronicMailAddress", &cfg.service.contact_email)?;
        w.write_event(Event::End(BytesEnd::new("ContactInformation")))
            .map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("Service"))).map_err(xml_err)?;

    // capability block
    w.write_event(Event::Start(BytesStart::new("Capability")))
        .map_err(xml_err)?;

    // request
    w.write_event(Event::Start(BytesStart::new("Request")))
        .map_err(xml_err)?;
    let advertised_formats = configured_formats(cfg);
    for op in ["GetCapabilities", "GetMap"] {
        w.write_event(Event::Start(BytesStart::new(op))).map_err(xml_err)?;
        for fmt in &advertised_formats {
            text_element(&mut w, "Format", fmt.mime())?;
        }
        w.write_event(Event::End(BytesEnd::new(op))).map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("Request"))).map_err(xml_err)?;

    // root layer
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(&mut w, "Title", &cfg.service.title)?;

    // canonical-only crs advertisement: we emit bbox values without a per-crs
    // transform, so listing every allowlist entry would lie about what's
    // available. once we round-trip BBOX through the reprojection allowlist
    // we can advertise the full set again.
    text_element(&mut w, "CRS", cfg.source.native_crs.as_str())?;

    if let Some(bb) = root_bbox {
        write_bbox(&mut w, cfg.source.native_crs.as_str(), bb)?;
    }

    // child layers
    for layer in &cfg.layers {
        w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
        text_element(&mut w, "Name", layer.name.as_str())?;
        text_element(&mut w, "Title", &layer.title)?;
        if !layer.abstract_.is_empty() {
            text_element(&mut w, "Abstract", &layer.abstract_)?;
        }
        let bbox = layer_bboxes.get(&layer.name).copied().or(layer.bbox);
        if let Some(bb) = bbox {
            write_bbox(&mut w, cfg.source.native_crs.as_str(), bb)?;
        }
        if let Some(scale) = &layer.scale {
            if let Some(min) = scale.min {
                text_element(&mut w, "MinScaleDenominator", &min.to_string())?;
            }
            if let Some(max) = scale.max {
                text_element(&mut w, "MaxScaleDenominator", &max.to_string())?;
            }
        }
        w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    }

    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("Capability")))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("WMS_Capabilities")))
        .map_err(xml_err)?;

    String::from_utf8(buf.into_inner()).map_err(|e| WmsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    })
}

/// derive a per-layer bbox by unioning the cells materialised in the manifest.
/// falls back to the layer's configured bbox when no artifact entry exists.
fn derive_layer_bboxes(cfg: &Config, manifest: &Manifest) -> HashMap<LayerId, Bbox> {
    let cell_sizes = cfg.cells.size_per_band_m().unwrap_or_default();
    let origin = (cfg.cells.origin[0], cfg.cells.origin[1]);

    let mut out: HashMap<LayerId, Bbox> = HashMap::new();
    for entry in &manifest.layer_artifacts {
        let Ok(ParsedArtifactKey::Layer { layer, cell }) = entry.key.parse() else {
            continue;
        };
        let Some(size) = cell_sizes.get(cell.band.as_str()).copied() else {
            continue;
        };
        // bbox of cell (x, y) at given band size, anchored at the grid origin.
        let min_x = origin.0 + (cell.x as f64) * size;
        let min_y = origin.1 + (cell.y as f64) * size;
        let bbox = Bbox::new(min_x, min_y, min_x + size, min_y + size);
        out.entry(layer)
            .and_modify(|b| *b = union_bbox(*b, bbox))
            .or_insert(bbox);
    }

    for layer in &cfg.layers {
        if let Some(bbox) = layer.bbox {
            out.entry(layer.name.clone()).or_insert(bbox);
        }
    }
    out
}

fn union_bbox(a: Bbox, b: Bbox) -> Bbox {
    Bbox::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}

/// Resolve the format set the runtime advertises. Falls back to PNG when
/// `interfaces.wms.formats` is omitted, matching `WmsConfig::from_config`.
fn configured_formats(cfg: &Config) -> Vec<ImageFormat> {
    let configured: Vec<ImageFormat> = cfg
        .interfaces
        .wms
        .as_ref()
        .map(|w| {
            w.formats
                .iter()
                .filter_map(|f| match f.as_str() {
                    "image/png" => Some(ImageFormat::Png),
                    "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec![ImageFormat::Png]
    } else {
        configured
    }
}

fn write_bbox<W: std::io::Write>(w: &mut Writer<W>, crs: &str, bbox: Bbox) -> Result<(), WmsError> {
    let minx = bbox.min_x.to_string();
    let miny = bbox.min_y.to_string();
    let maxx = bbox.max_x.to_string();
    let maxy = bbox.max_y.to_string();
    let mut bb = BytesStart::new("BoundingBox");
    bb.push_attribute(("CRS", crs));
    bb.push_attribute(("minx", minx.as_str()));
    bb.push_attribute(("miny", miny.as_str()));
    bb.push_attribute(("maxx", maxx.as_str()));
    bb.push_attribute(("maxy", maxy.as_str()));
    w.write_event(Event::Empty(bb)).map_err(xml_err)
}

fn text_element<W: std::io::Write>(w: &mut Writer<W>, name: &str, text: &str) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new(name))).map_err(xml_err)?;
    w.write_event(Event::Text(BytesText::new(text))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new(name))).map_err(xml_err)?;
    Ok(())
}

fn xml_err(e: std::io::Error) -> WmsError {
    WmsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use mars_types::{ArtifactEntry, ArtifactKey, Cell, ContentHash, ScaleBand};
    use quick_xml::Reader;

    fn minimal_cfg() -> Config {
        // build via yaml so we don't have to hand-roll every field
        let yaml = r#"
service: { name: t, title: "T", abstract: "A", contact_email: ops@x }
source: { type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp/c, max_size: 1GiB }
scales:
  bands: [{ name: hi, max_denom_exclusive: 25000 }]
cells:
  grid: regular
  origin: [0, 0]
  size_per_band: { hi: 1024m }
interfaces: {}
reprojection:
  allowlist: [EPSG:25832, EPSG:4326]
layers:
  - name: a
    title: "A layer"
    type: polygon
    sources:
      - { from: t, geometry_column: g }
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn parses_clean() {
        let cfg = minimal_cfg();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("WMS_Capabilities"));
        assert!(xml.contains("EPSG:25832"));
        assert!(xml.contains("<Name>a</Name>"));

        // structural validation
        let mut r = Reader::from_str(&xml);
        let mut depth: i32 = 0;
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf).unwrap() {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        assert_eq!(depth, 0);
    }

    #[test]
    fn escapes_xml_special_chars() {
        let mut cfg = minimal_cfg();
        cfg.layers[0].title = "A & B <C>".into();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        // special chars must be escaped, not raw
        assert!(!xml.contains("A & B <C>"), "raw unescaped special chars found");
        assert!(xml.contains("A &amp; B &lt;C&gt;"), "expected escaped entities");
    }

    #[test]
    fn empty_layers_produces_valid_xml() {
        let mut cfg = minimal_cfg();
        cfg.layers.clear();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Layer>"));
        assert!(xml.contains("</Layer>"));

        let mut r = Reader::from_str(&xml);
        let mut depth: i32 = 0;
        let mut buf = Vec::new();
        loop {
            match r.read_event_into(&mut buf).unwrap() {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
        assert_eq!(depth, 0);
    }

    #[test]
    fn advertises_only_canonical_crs() {
        let cfg = minimal_cfg();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        // we only emit values in the canonical crs, so the advertised CRS
        // list and BoundingBox CRS must reflect only that.
        assert!(xml.contains("<CRS>EPSG:25832</CRS>"));
        assert!(!xml.contains("<CRS>EPSG:4326</CRS>"));
        assert!(!xml.contains(r#"CRS="EPSG:4326""#));
    }

    #[test]
    fn omits_contact_when_email_empty() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(
            !xml.contains("ContactInformation"),
            "expected no contact block when email empty"
        );
    }

    #[test]
    fn advertises_configured_formats() {
        let yaml = r#"
service: { name: t, title: T, abstract: A, contact_email: "" }
source: { type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp/c, max_size: 1GiB }
scales:
  bands: [{ name: hi, max_denom_exclusive: 25000 }]
cells:
  grid: regular
  origin: [0, 0]
  size_per_band: { hi: 1024m }
interfaces:
  wms:
    enabled: true
    formats: ["image/png", "image/jpeg"]
reprojection:
  allowlist: [EPSG:25832]
layers:
  - { name: a, title: A, type: polygon, sources: [{ from: t, geometry_column: g }] }
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Format>image/png</Format>"));
        assert!(xml.contains("<Format>image/jpeg</Format>"));
    }

    #[test]
    fn emits_canonical_bbox_only() {
        let cfg = minimal_cfg();
        let layer = LayerId::new("a");
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };
        let key = ArtifactKey::try_build_layer(&layer, &cell, ContentHash::zero()).unwrap();
        let entry = ArtifactEntry {
            key,
            hash: ContentHash::zero(),
            size_bytes: 0,
        };
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![entry], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        // root + child layer = 2 BoundingBox elements, both in canonical crs.
        assert_eq!(xml.matches(r#"CRS="EPSG:25832""#).count(), 2, "{xml}");
        assert_eq!(xml.matches(r#"CRS="EPSG:4326""#).count(), 0, "{xml}");
    }

    #[test]
    fn derives_layer_bbox_from_manifest_cells() {
        let cfg = minimal_cfg();
        let layer = LayerId::new("a");
        let cell = Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        };
        let key = ArtifactKey::try_build_layer(&layer, &cell, ContentHash::zero()).unwrap();
        let entry = ArtifactEntry {
            key,
            hash: ContentHash::zero(),
            size_bytes: 0,
        };
        let m = Manifest::new(1, cfg.service.name.clone(), vec![], vec![entry], None, vec![]);
        let xml = capabilities_xml(&cfg, &m).unwrap();
        // cell (0,0) at 1024m should produce a bbox of (0,0,1024,1024).
        assert!(xml.contains("minx=\"0\""), "missing minx=0: {xml}");
        assert!(xml.contains("maxx=\"1024\""), "missing maxx=1024: {xml}");
    }
}
