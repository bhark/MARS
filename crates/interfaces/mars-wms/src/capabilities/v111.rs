//! WMS 1.1.1 GetCapabilities document.
//!
//! The 1.1.1 envelope predates the 1.3.0 redesign so several wire details
//! shift: root element is `<WMT_MS_Capabilities>`, the per-layer CRS list
//! lives under `<SRS>` (not `<CRS>`), `<BoundingBox>` carries an `SRS`
//! attribute, and the geographic extent is `<LatLonBoundingBox>` (replaced
//! in 1.3.0 by `EX_GeographicBoundingBox`). Scale gating uses
//! `<ScaleHint>` in 1.1.1 but units differ from
//! `<MinScaleDenominator>/<MaxScaleDenominator>` and translation requires
//! knowing the standardised pixel size; we omit ScaleHint here for now
//! rather than emit a misleading value.

use std::io::Cursor;

use mars_config::Config;
use mars_types::{Bbox, ImageFormat, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use super::{INFO_FORMATS, configured_formats, derive_layer_bboxes, text_element, union_bbox, xml_err};
use crate::WmsError;

/// Render the WMS 1.1.1 capabilities XML.
pub(super) fn capabilities_xml(cfg: &Config, manifest: &Manifest) -> Result<String, WmsError> {
    let layer_bboxes = derive_layer_bboxes(cfg, manifest);
    let root_bbox = layer_bboxes.values().copied().reduce(union_bbox);

    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .map_err(xml_err)?;

    let mut root = BytesStart::new("WMT_MS_Capabilities");
    root.push_attribute(("version", "1.1.1"));
    w.write_event(Event::Start(root)).map_err(xml_err)?;

    // service block
    w.write_event(Event::Start(BytesStart::new("Service")))
        .map_err(xml_err)?;
    text_element(&mut w, "Name", "OGC:WMS")?;
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

    w.write_event(Event::Start(BytesStart::new("Request")))
        .map_err(xml_err)?;
    let advertised_formats = configured_formats(cfg);
    // 1.1.1 GetCapabilities advertises the wms_xml capabilities format
    // alongside the renderable raster formats so clients can negotiate
    // the metadata document.
    w.write_event(Event::Start(BytesStart::new("GetCapabilities")))
        .map_err(xml_err)?;
    text_element(&mut w, "Format", "application/vnd.ogc.wms_xml")?;
    w.write_event(Event::End(BytesEnd::new("GetCapabilities")))
        .map_err(xml_err)?;
    w.write_event(Event::Start(BytesStart::new("GetMap")))
        .map_err(xml_err)?;
    for fmt in &advertised_formats {
        text_element(&mut w, "Format", fmt.mime())?;
    }
    w.write_event(Event::End(BytesEnd::new("GetMap"))).map_err(xml_err)?;
    if cfg.layers.iter().any(|l| l.enable_get_feature_info) {
        w.write_event(Event::Start(BytesStart::new("GetFeatureInfo")))
            .map_err(xml_err)?;
        for mime in INFO_FORMATS {
            text_element(&mut w, "Format", mime)?;
        }
        w.write_event(Event::End(BytesEnd::new("GetFeatureInfo")))
            .map_err(xml_err)?;
    }
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

    // canonical-only srs advertisement: bbox values are emitted without a
    // per-srs transform, so listing every allowlist entry would lie about
    // what's available.
    text_element(&mut w, "SRS", cfg.source.native_crs.as_str())?;

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
        write_default_style_with_legend_url(&mut w, layer, &advertised_formats)?;
        w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    }

    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("Capability")))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("WMT_MS_Capabilities")))
        .map_err(xml_err)?;

    String::from_utf8(buf.into_inner()).map_err(|e| WmsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    })
}

/// 1.1.1 default `<Style>` block. The LegendURL template pins
/// `version=1.1.1` so the client round-trips the version it asked for.
fn write_default_style_with_legend_url<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Style"))).map_err(xml_err)?;
    text_element(w, "Name", "default")?;
    text_element(w, "Title", "Default style")?;
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
                "?service=WMS&version=1.1.1&request=GetLegendGraphic&layer={}&format={}",
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

/// 1.1.1 `<BoundingBox SRS="...">` (not `CRS=`). Axis order is east/north
/// on the wire regardless of CRS declaration, so the same field order is
/// used for all CRSes.
fn write_bbox<W: std::io::Write>(w: &mut Writer<W>, srs: &str, bbox: Bbox) -> Result<(), WmsError> {
    let minx = bbox.min_x.to_string();
    let miny = bbox.min_y.to_string();
    let maxx = bbox.max_x.to_string();
    let maxy = bbox.max_y.to_string();
    let mut bb = BytesStart::new("BoundingBox");
    bb.push_attribute(("SRS", srs));
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
    fn uses_111_root_element() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<WMT_MS_Capabilities"));
        assert!(xml.contains(r#"version="1.1.1""#));
        assert!(!xml.contains("<WMS_Capabilities"));
    }

    #[test]
    fn srs_not_crs() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<SRS>EPSG:25832</SRS>"));
        assert!(!xml.contains("<CRS>"));
    }

    #[test]
    fn legend_url_pins_111() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("version=1.1.1"));
        assert!(!xml.contains("version=1.3.0"));
    }

    #[test]
    fn parses_clean() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
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
    fn bbox_attribute_uses_srs_and_lowercase_attrs() {
        // 1.1.1 BoundingBox uses lowercase minx/miny/maxx/maxy. v130 uses
        // the same spellings but with a CRS= attribute - we lock both so an
        // accidental cross-pollination of attribute shape between emitters
        // shows up here.
        let mut cfg = minimal_cfg();
        cfg.layers[0].bbox = Some(mars_types::Bbox::new(0.0, 0.0, 10.0, 10.0));
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains(r#"SRS="EPSG:25832""#));
        assert!(!xml.contains(r#"CRS="EPSG:25832""#));
        for attr in ["minx=", "miny=", "maxx=", "maxy="] {
            assert!(xml.contains(attr), "missing attr {attr} in {xml}");
        }
    }

    #[test]
    fn escapes_xml_special_chars_111() {
        let mut cfg = minimal_cfg();
        cfg.layers[0].title = "A & B <C>".into();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("A & B <C>"), "raw unescaped special chars found");
        assert!(xml.contains("A &amp; B &lt;C&gt;"), "expected escaped entities");
    }

    #[test]
    fn empty_layers_produces_valid_xml_111() {
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
    fn queryable_layer_emits_queryable_attribute_111() {
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
    fn no_queryable_layers_skips_get_feature_info_111() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("<GetFeatureInfo>"));
        assert!(!xml.contains(r#"queryable="1""#));
    }

    #[test]
    fn legend_advertised_in_request_block_and_per_layer_111() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<GetLegendGraphic>"));
        assert!(xml.contains("<LegendURL"));
        assert!(xml.contains("request=GetLegendGraphic"));
        assert!(xml.contains("layer=a"));
        // 1.1.1-specific: the LegendURL must round-trip the negotiated
        // version, not the 1.3.0 default.
        assert!(xml.contains("version=1.1.1"));
    }

    #[test]
    fn advertises_only_canonical_srs_111() {
        // mirror of v130's advertises_only_canonical_crs but pinned to the
        // 1.1.1 <SRS> spelling; the reprojection allowlist includes
        // EPSG:4326 yet we must only advertise the native crs.
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<SRS>EPSG:25832</SRS>"));
        assert!(!xml.contains("<SRS>EPSG:4326</SRS>"));
        assert!(!xml.contains(r#"SRS="EPSG:4326""#));
        assert!(!xml.contains("<CRS>"));
    }

    #[test]
    fn omits_contact_when_email_empty_111() {
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
    fn advertises_configured_formats_111() {
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
        // 1.1.1 also advertises the wms_xml capabilities document format
        // under <GetCapabilities>; assert it survives the configured-format
        // override path.
        assert!(xml.contains("application/vnd.ogc.wms_xml"));
    }
}
