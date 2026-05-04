//! WMS 1.3.0 GetCapabilities document. SPEC §12.
//!
//! We render a minimal, valid 1.3.0 capabilities body; format conformance is a
//! Phase 2 concern. The output is built once at startup and served verbatim.

use std::io::Cursor;

use mars_config::Config;
use mars_types::Manifest;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

use crate::WmsError;

/// Render the capabilities XML. The manifest is currently informational; we use
/// it to short-circuit when no layers are present.
pub fn capabilities_xml(cfg: &Config, _manifest: &Manifest) -> Result<String, WmsError> {
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
    for op in ["GetCapabilities", "GetMap"] {
        w.write_event(Event::Start(BytesStart::new(op))).map_err(xml_err)?;
        text_element(&mut w, "Format", "image/png")?;
        w.write_event(Event::End(BytesEnd::new(op))).map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("Request"))).map_err(xml_err)?;

    // root layer
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(&mut w, "Title", &cfg.service.title)?;

    // crs allowlist
    for crs in &cfg.reprojection.allowlist {
        text_element(&mut w, "CRS", crs.as_str())?;
    }

    // child layers
    for layer in &cfg.layers {
        w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
        text_element(&mut w, "Name", layer.name.as_str())?;
        text_element(&mut w, "Title", &layer.title)?;
        if !layer.abstract_.is_empty() {
            text_element(&mut w, "Abstract", &layer.abstract_)?;
        }
        if let Some(bbox) = &layer.bbox {
            let mut bb = BytesStart::new("BoundingBox");
            bb.push_attribute(("CRS", cfg.source.native_crs.as_str()));
            bb.push_attribute(("minx", bbox.min_x.to_string().as_str()));
            bb.push_attribute(("miny", bbox.min_y.to_string().as_str()));
            bb.push_attribute(("maxx", bbox.max_x.to_string().as_str()));
            bb.push_attribute(("maxy", bbox.max_y.to_string().as_str()));
            w.write_event(Event::Empty(bb)).map_err(xml_err)?;
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
  bands: [{ name: hi, max_denom: 25000 }]
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
        serde_yml::from_str(yaml).unwrap()
    }

    #[test]
    fn parses_clean() {
        let cfg = minimal_cfg();
        let m = Manifest {
            version: 1,
            service: cfg.service.name.clone(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
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
        let m = Manifest {
            version: 1,
            service: cfg.service.name.clone(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
        let xml = capabilities_xml(&cfg, &m).unwrap();
        // special chars must be escaped, not raw
        assert!(!xml.contains("A & B <C>"), "raw unescaped special chars found");
        assert!(xml.contains("A &amp; B &lt;C&gt;"), "expected escaped entities");
    }

    #[test]
    fn empty_layers_produces_valid_xml() {
        let mut cfg = minimal_cfg();
        cfg.layers.clear();
        let m = Manifest {
            version: 1,
            service: cfg.service.name.clone(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
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
    fn empty_allowlist_omits_crs() {
        let mut cfg = minimal_cfg();
        cfg.reprojection.allowlist.clear();
        let m = Manifest {
            version: 1,
            service: cfg.service.name.clone(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("<CRS>"), "expected no CRS elements when allowlist is empty");
    }

    #[test]
    fn omits_contact_when_email_empty() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        let m = Manifest {
            version: 1,
            service: cfg.service.name.clone(),
            source_artifacts: vec![],
            layer_artifacts: vec![],
            style_artifact: None,
        };
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("ContactInformation"), "expected no contact block when email empty");
    }
}
