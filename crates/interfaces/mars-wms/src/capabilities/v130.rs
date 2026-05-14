//! WMS 1.3.0 GetCapabilities document.

use std::io::Cursor;

use mars_config::Config;
use mars_types::{Bbox, ImageFormat, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use super::{INFO_FORMATS, configured_formats, derive_layer_bboxes, text_element, union_bbox, xml_err};
use crate::WmsError;

/// Render the WMS 1.3.0 capabilities XML.
pub(super) fn capabilities_xml(cfg: &Config, manifest: &Manifest) -> Result<String, WmsError> {
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
    // gfi advertised when at least one layer opts in; emitting it
    // unconditionally would invite identify clicks that always come back empty.
    if cfg.layers.iter().any(|l| l.enable_get_feature_info) {
        w.write_event(Event::Start(BytesStart::new("GetFeatureInfo")))
            .map_err(xml_err)?;
        for mime in INFO_FORMATS {
            text_element(&mut w, "Format", mime)?;
        }
        w.write_event(Event::End(BytesEnd::new("GetFeatureInfo")))
            .map_err(xml_err)?;
    }
    // legend graphic is always available; the runtime can compose a default
    // swatch even for class-less layers.
    w.write_event(Event::Start(BytesStart::new("GetLegendGraphic")))
        .map_err(xml_err)?;
    for fmt in &advertised_formats {
        text_element(&mut w, "Format", fmt.mime())?;
    }
    w.write_event(Event::End(BytesEnd::new("GetLegendGraphic")))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("Request"))).map_err(xml_err)?;

    // root layer
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(&mut w, "Title", &cfg.service.title)?;

    // canonical-only crs advertisement: bbox values are emitted without a
    // per-crs transform, so listing every allowlist entry would lie about
    // what's available. once BBOX round-trips through the reprojection
    // allowlist the full set can be advertised again.
    text_element(&mut w, "CRS", cfg.source.native_crs.as_str())?;

    if let Some(bb) = root_bbox {
        write_bbox(&mut w, cfg.source.native_crs.as_str(), bb)?;
    }

    // child layers
    for layer in &cfg.layers {
        let mut layer_tag = BytesStart::new("Layer");
        if layer.enable_get_feature_info {
            layer_tag.push_attribute(("queryable", "1"));
        }
        w.write_event(Event::Start(layer_tag)).map_err(xml_err)?;
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
        write_default_style_with_legend_url(&mut w, layer, &advertised_formats)?;
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

/// Emit a single default `<Style>` block per layer including a relative
/// LegendURL. Path matches the runtime route; clients ground it on the
/// request URL they reached the service on.
fn write_default_style_with_legend_url<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Style"))).map_err(xml_err)?;
    text_element(w, "Name", "default")?;
    text_element(w, "Title", "Default style")?;
    // one LegendURL per format we advertise; ~20 px default mirrors MapServer.
    for fmt in formats {
        let mut legend = BytesStart::new("LegendURL");
        legend.push_attribute(("width", "20"));
        legend.push_attribute(("height", "20"));
        w.write_event(Event::Start(legend)).map_err(xml_err)?;
        text_element(w, "Format", fmt.mime())?;
        let mut online = BytesStart::new("OnlineResource");
        online.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
        online.push_attribute(("xlink:type", "simple"));
        online.push_attribute((
            "xlink:href",
            format!(
                "?service=WMS&version=1.3.0&request=GetLegendGraphic&layer={}&format={}",
                layer.name.as_str(),
                fmt.mime()
            )
            .as_str(),
        ));
        w.write_event(Event::Empty(online)).map_err(xml_err)?;
        w.write_event(Event::End(BytesEnd::new("LegendURL"))).map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("Style"))).map_err(xml_err)?;
    Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use quick_xml::Reader;

    use super::*;

    fn minimal_cfg() -> Config {
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
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("WMS_Capabilities"));
        assert!(xml.contains("EPSG:25832"));
        assert!(xml.contains("<Name>a</Name>"));

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
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("A & B <C>"), "raw unescaped special chars found");
        assert!(xml.contains("A &amp; B &lt;C&gt;"), "expected escaped entities");
    }

    #[test]
    fn empty_layers_produces_valid_xml() {
        let mut cfg = minimal_cfg();
        cfg.layers.clear();
        let m = Manifest::empty(1, cfg.service.name.clone());
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
    fn queryable_layer_emits_queryable_attribute() {
        let mut cfg = minimal_cfg();
        cfg.layers[0].enable_get_feature_info = true;
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains(r#"<Layer queryable="1">"#));
        assert!(xml.contains("<GetFeatureInfo>"));
        assert!(xml.contains("text/plain"));
        assert!(xml.contains("application/json"));
    }

    #[test]
    fn legend_advertised_in_request_block_and_per_layer() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<GetLegendGraphic>"));
        assert!(xml.contains("<LegendURL"));
        assert!(xml.contains("request=GetLegendGraphic"));
        assert!(xml.contains("layer=a"));
    }

    #[test]
    fn no_queryable_layers_skips_get_feature_info() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("<GetFeatureInfo>"));
        assert!(!xml.contains(r#"queryable="1""#));
    }

    #[test]
    fn advertises_only_canonical_crs() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<CRS>EPSG:25832</CRS>"));
        assert!(!xml.contains("<CRS>EPSG:4326</CRS>"));
        assert!(!xml.contains(r#"CRS="EPSG:4326""#));
    }

    #[test]
    fn omits_contact_when_email_empty() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        let m = Manifest::empty(1, cfg.service.name.clone());
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
    formats: ["image/png", "image/jpeg", "image/webp"]
reprojection:
  allowlist: [EPSG:25832]
layers:
  - { name: a, title: A, type: polygon, sources: [{ from: t, geometry_column: g }] }
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Format>image/png</Format>"));
        assert!(xml.contains("<Format>image/jpeg</Format>"));
        assert!(xml.contains("<Format>image/webp</Format>"));
    }
}
