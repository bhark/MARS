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
use mars_ows_common::{text_element, xml_err};
use mars_types::{Bbox, ImageFormat, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use super::{
    INFO_FORMATS, LayerNode, StyleAd, build_layer_tree, configured_formats, derive_layer_bboxes,
    resolved_request_formats, style_advertisements, union_bbox, write_contact_information, write_dcp_type,
    write_keyword_list, write_online_resource,
};
use crate::WmsError;

/// Render the WMS 1.1.1 capabilities XML.
pub(super) fn capabilities_xml(cfg: &Config, manifest: &Manifest) -> Result<String, WmsError> {
    let layer_bboxes = derive_layer_bboxes(cfg, manifest);
    let root_bbox = layer_bboxes.values().copied().reduce(union_bbox);

    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new(
        "1.0",
        Some(cfg.service.ows.xml_encoding()),
        None,
    )))
    .map_err(xml_err)?;

    let mut root = BytesStart::new("WMT_MS_Capabilities");
    root.push_attribute(("version", "1.1.1"));
    w.write_event(Event::Start(root)).map_err(xml_err)?;

    // service block; spec ordering identical to 1.3.0: Name, Title, Abstract,
    // KeywordList, OnlineResource, ContactInformation, Fees, AccessConstraints.
    w.write_event(Event::Start(BytesStart::new("Service")))
        .map_err(xml_err)?;
    text_element(&mut w, "Name", "OGC:WMS")?;
    text_element(&mut w, "Title", &cfg.service.title)?;
    text_element(&mut w, "Abstract", &cfg.service.abstract_)?;
    write_keyword_list(&mut w, &cfg.service.ows.keywords)?;
    if let Some(href) = cfg.service.ows.online_resource.as_deref() {
        write_online_resource(&mut w, href)?;
    }
    write_contact_information(&mut w, &cfg.service.contact, &cfg.service.contact_email)?;
    if let Some(fees) = cfg.service.ows.fees.as_deref() {
        text_element(&mut w, "Fees", fees)?;
    }
    if let Some(ac) = cfg.service.ows.access_constraints.as_deref() {
        text_element(&mut w, "AccessConstraints", ac)?;
    }
    w.write_event(Event::End(BytesEnd::new("Service"))).map_err(xml_err)?;

    // capability block
    w.write_event(Event::Start(BytesStart::new("Capability")))
        .map_err(xml_err)?;

    w.write_event(Event::Start(BytesStart::new("Request")))
        .map_err(xml_err)?;
    let advertised_formats = configured_formats(cfg);
    let online_href = cfg.service.ows.online_resource.as_deref();
    let getmap_formats = resolved_request_formats(&cfg.service.wms.formats.get_map, &advertised_formats);
    let getlegend_formats = resolved_request_formats(&cfg.service.wms.formats.get_legend_graphic, &advertised_formats);
    let getfi_formats: Vec<String> = if cfg.service.wms.formats.get_feature_info.is_empty() {
        INFO_FORMATS.iter().map(|s| (*s).to_string()).collect()
    } else {
        cfg.service.wms.formats.get_feature_info.clone()
    };
    // 1.1.1 GetCapabilities advertises the wms_xml capabilities document
    // format so clients can negotiate the metadata response; emitted before
    // any service-level format override.
    write_request_op_111(
        &mut w,
        "GetCapabilities",
        std::iter::once("application/vnd.ogc.wms_xml".to_string())
            .collect::<Vec<_>>()
            .as_slice(),
        online_href,
    )?;
    write_request_op_111(&mut w, "GetMap", &getmap_formats, online_href)?;
    if cfg
        .layers
        .iter()
        .any(|l| l.permits_op(mars_config::ServiceOp::WmsGetFeatureInfo))
    {
        write_request_op_111(&mut w, "GetFeatureInfo", &getfi_formats, online_href)?;
    }
    write_request_op_111(&mut w, "GetLegendGraphic", &getlegend_formats, online_href)?;
    w.write_event(Event::End(BytesEnd::new("Request"))).map_err(xml_err)?;

    // root layer
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(&mut w, "Title", &cfg.service.title)?;

    // canonical-only srs advertisement: bbox values are emitted without a
    // per-srs transform, so listing every allowlist entry would lie about
    // what's available. multi-source configs expand the root-layer SRS set
    // to every distinct native crs declared across cfg.sources.
    for srs in super::distinct_native_crses(cfg) {
        text_element(&mut w, "SRS", srs)?;
    }
    let root_crs = super::service_root_native_crs(cfg);
    for srs in &cfg.service.wms.advertised_crs {
        if super::distinct_native_crses(cfg).contains(&srs.as_str()) {
            continue;
        }
        text_element(&mut w, "SRS", srs)?;
    }

    if let Some(bb) = root_bbox {
        write_bbox(&mut w, root_crs, bb)?;
    }

    // 1.1.1 has no inheritable root-layer AuthorityURL or Identifier - those
    // elements exist only at per-layer scope in this version (1.3.0 added
    // the root-layer inheritance). service.wms.authorities and service.wms.identifiers
    // therefore have no 1.1.1 surface.

    // child layers: walk the path-derived tree so configured `group` values
    // surface as nested <Layer> elements.
    let tree = build_layer_tree(&cfg.layers);
    emit_children(&mut w, &tree, cfg, &layer_bboxes, &advertised_formats)?;

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

/// Per-operation 1.1.1 Request emit. Same DCPType shape as 1.3.0.
fn write_request_op_111<W: std::io::Write>(
    w: &mut Writer<W>,
    op: &str,
    formats: &[String],
    online_href: Option<&str>,
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new(op))).map_err(xml_err)?;
    for fmt in formats {
        text_element(w, "Format", fmt)?;
    }
    if let Some(href) = online_href {
        write_dcp_type(w, href)?;
    }
    w.write_event(Event::End(BytesEnd::new(op))).map_err(xml_err)?;
    Ok(())
}

/// Walk one tree level emitting group nodes first (sorted by segment)
/// then real layer leaves (config order). Children of synthesised group
/// nodes inherit the root SRS/BoundingBox per WMS 1.1.1 spec, so interior
/// nodes emit only a Title.
fn emit_children<W: std::io::Write>(
    w: &mut Writer<W>,
    node: &LayerNode<'_>,
    cfg: &Config,
    layer_bboxes: &std::collections::HashMap<mars_types::LayerId, Bbox>,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    for child in node.group_children.values() {
        emit_group(w, child, cfg, layer_bboxes, formats)?;
    }
    for child in &node.layer_children {
        if let Some(layer) = child.leaf {
            if !layer.permits_op(mars_config::ServiceOp::WmsGetCapabilities) {
                continue;
            }
            emit_leaf(w, layer, cfg, layer_bboxes, formats)?;
        }
    }
    Ok(())
}

fn emit_group<W: std::io::Write>(
    w: &mut Writer<W>,
    node: &LayerNode<'_>,
    cfg: &Config,
    layer_bboxes: &std::collections::HashMap<mars_types::LayerId, Bbox>,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(w, "Title", &node.title)?;
    emit_children(w, node, cfg, layer_bboxes, formats)?;
    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    Ok(())
}

fn emit_leaf<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    cfg: &Config,
    layer_bboxes: &std::collections::HashMap<mars_types::LayerId, Bbox>,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    let mut layer_tag = BytesStart::new("Layer");
    if layer.permits_op(mars_config::ServiceOp::WmsGetFeatureInfo) {
        layer_tag.push_attribute(("queryable", "1"));
    }
    if layer.wms.opaque {
        layer_tag.push_attribute(("opaque", "1"));
    }
    w.write_event(Event::Start(layer_tag)).map_err(xml_err)?;
    // 1.1.1 §7.1.4.5.2: same as 1.3.0 - omitting <Name> marks the layer as
    // a metadata-only container. Hide it when GetMap is denied.
    if layer.permits_op(mars_config::ServiceOp::WmsGetMap) {
        text_element(w, "Name", layer.name.as_str())?;
    }
    text_element(w, "Title", &layer.title)?;
    if !layer.abstract_.is_empty() {
        text_element(w, "Abstract", &layer.abstract_)?;
    }
    write_keyword_list(w, &layer.ows.keywords)?;
    // per-layer advertised SRS list; 1.1.1 uses <SRS> not <CRS>.
    if let Some(srses) = layer.wms.advertised_crs.as_ref() {
        for s in srses {
            text_element(w, "SRS", s)?;
        }
    }
    let bbox = layer_bboxes.get(&layer.name).copied().or(layer.bbox);
    if let Some(bb) = bbox {
        write_bbox(w, super::layer_native_crs(cfg, layer), bb)?;
    }
    // 1.1.1 layer-block ordering: MetadataURL before Attribution / AuthorityURL
    // / Identifier (opposite of 1.3.0).
    for mu in &layer.ows.metadata_urls {
        write_metadata_url_111(w, mu)?;
    }
    if let Some(attr) = layer.ows.attribution.as_ref() {
        write_attribution_111(w, attr)?;
    }
    for auth in &layer.ows.authorities {
        write_authority_url_111(w, &auth.name, &auth.href)?;
    }
    for ident in &layer.ows.identifiers {
        write_identifier_111(w, &ident.authority, &ident.value)?;
    }
    write_styles_with_legend_urls(w, layer, formats)?;
    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    Ok(())
}

fn write_attribution_111<W: std::io::Write>(
    w: &mut Writer<W>,
    attr: &mars_config::Attribution,
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Attribution")))
        .map_err(xml_err)?;
    if !attr.title.is_empty() {
        text_element(w, "Title", &attr.title)?;
    }
    if let Some(href) = attr.online_resource.as_deref() {
        write_online_resource(w, href)?;
    }
    if let Some(logo) = attr.logo.as_ref() {
        let mut lu = BytesStart::new("LogoURL");
        if let Some(width) = logo.width {
            lu.push_attribute(("width", width.to_string().as_str()));
        }
        if let Some(height) = logo.height {
            lu.push_attribute(("height", height.to_string().as_str()));
        }
        w.write_event(Event::Start(lu)).map_err(xml_err)?;
        text_element(w, "Format", &logo.format)?;
        write_online_resource(w, &logo.href)?;
        w.write_event(Event::End(BytesEnd::new("LogoURL"))).map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("Attribution")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_metadata_url_111<W: std::io::Write>(w: &mut Writer<W>, mu: &mars_config::MetadataUrl) -> Result<(), WmsError> {
    let mut tag = BytesStart::new("MetadataURL");
    tag.push_attribute(("type", mu.type_.as_str()));
    w.write_event(Event::Start(tag)).map_err(xml_err)?;
    text_element(w, "Format", &mu.format)?;
    write_online_resource(w, &mu.href)?;
    w.write_event(Event::End(BytesEnd::new("MetadataURL")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_authority_url_111<W: std::io::Write>(w: &mut Writer<W>, name: &str, href: &str) -> Result<(), WmsError> {
    let mut au = BytesStart::new("AuthorityURL");
    au.push_attribute(("name", name));
    w.write_event(Event::Start(au)).map_err(xml_err)?;
    write_online_resource(w, href)?;
    w.write_event(Event::End(BytesEnd::new("AuthorityURL")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_identifier_111<W: std::io::Write>(w: &mut Writer<W>, authority: &str, value: &str) -> Result<(), WmsError> {
    let mut id = BytesStart::new("Identifier");
    id.push_attribute(("authority", authority));
    w.write_event(Event::Start(id)).map_err(xml_err)?;
    w.write_event(Event::Text(quick_xml::events::BytesText::new(value)))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("Identifier")))
        .map_err(xml_err)?;
    Ok(())
}

/// 1.1.1 per-class `<Style>` blocks. The LegendURL template pins
/// `version=1.1.1` so the client round-trips the version it asked for, and
/// each entry carries its `rule=<class>` so the runtime renders that class
/// only.
fn write_styles_with_legend_urls<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    for ad in style_advertisements(layer) {
        write_style_block_111(w, layer, &ad, formats)?;
    }
    Ok(())
}

fn write_style_block_111<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    ad: &StyleAd,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Style"))).map_err(xml_err)?;
    text_element(w, "Name", &ad.name)?;
    text_element(w, "Title", &ad.title)?;
    for fmt in formats {
        let mut legend = BytesStart::new("LegendURL");
        legend.push_attribute(("width", "20"));
        legend.push_attribute(("height", "20"));
        w.write_event(Event::Start(legend)).map_err(xml_err)?;
        text_element(w, "Format", fmt.mime())?;
        let mut online = BytesStart::new("OnlineResource");
        online.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
        online.push_attribute(("xlink:type", "simple"));
        let mut href = format!(
            "?service=WMS&version=1.1.1&request=GetLegendGraphic&layer={}&format={}",
            layer.name.as_str(),
            fmt.mime()
        );
        if let Some(rule) = ad.rule.as_deref() {
            href.push_str("&rule=");
            href.push_str(rule);
        }
        online.push_attribute(("xlink:href", href.as_str()));
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
sources:
  - { id: default, type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
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
        cfg.layers[0].wms.enable_get_feature_info = true;
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
    fn keywords_online_resource_fees_access_constraints_111() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.keywords = vec!["a".into(), "b".into()];
        cfg.service.ows.online_resource = Some("https://w.example/?".into());
        cfg.service.ows.fees = Some("none".into());
        cfg.service.ows.access_constraints = Some("CC0".into());
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<KeywordList>"));
        assert!(xml.contains("<Keyword>a</Keyword>"));
        assert!(xml.contains(r#"xlink:href="https://w.example/?""#));
        assert!(xml.contains("<Fees>none</Fees>"));
        assert!(xml.contains("<AccessConstraints>CC0</AccessConstraints>"));
    }

    #[test]
    fn full_contact_block_emitted_111() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        cfg.service.contact = mars_config::ContactInfo {
            person: "P".into(),
            organization: "O".into(),
            email: "e@x".into(),
            ..Default::default()
        };
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<ContactPerson>P</ContactPerson>"));
        assert!(xml.contains("<ContactOrganization>O</ContactOrganization>"));
        assert!(xml.contains("<ContactElectronicMailAddress>e@x</ContactElectronicMailAddress>"));
    }

    #[test]
    fn xml_encoding_honored_111() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.encoding = Some("ISO-8859-1".into());
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="ISO-8859-1""#));
    }

    #[test]
    fn dcp_type_emitted_when_online_resource_set_111() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.online_resource = Some("https://w.example/?".into());
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<DCPType>"));
        assert!(xml.contains("<HTTP>"));
        assert!(xml.contains("<Get>"));
    }

    #[test]
    fn root_layer_omits_authority_and_identifier_in_111() {
        // 1.3.0 emits AuthorityURL / Identifier inheritable from the root
        // layer. 1.1.1 has no equivalent root-scope inheritance, so the
        // service-level refs must not surface here.
        let mut cfg = minimal_cfg();
        cfg.service.wms.authorities = vec![mars_config::AuthorityRef {
            name: "iso".into(),
            href: "https://example.org/auth".into(),
        }];
        cfg.service.wms.identifiers = vec![mars_config::IdentifierRef {
            authority: "iso".into(),
            value: "urn:abc".into(),
        }];
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(!xml.contains("<AuthorityURL"));
        assert!(!xml.contains("<Identifier"));
    }

    #[test]
    fn advertised_srs_adds_to_root_layer_dedup_against_native_111() {
        let mut cfg = minimal_cfg();
        cfg.service.wms.advertised_crs = vec!["EPSG:25832".into(), "EPSG:3857".into()];
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert_eq!(xml.matches("<SRS>EPSG:25832</SRS>").count(), 1);
        assert!(xml.contains("<SRS>EPSG:3857</SRS>"));
    }

    #[test]
    fn advertises_configured_formats_111() {
        let yaml = r#"
service: { name: t, title: T, abstract: A, contact_email: "" }
sources:
  - { id: default, type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
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

    fn cfg_with_classes() -> Config {
        let yaml = r##"
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
interfaces: {}
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: roads
    title: "Roads"
    type: polygon
    sources: [{ from: t, geometry_column: g }]
    classes:
      - { name: main, title: "Main", style: { type: inline, fill: "#aabbcc" } }
      - { name: minor, title: "Minor", style: { type: inline, stroke: "#555555" } }
"##;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn emits_one_style_per_class_with_rule_param_111() {
        let cfg = cfg_with_classes();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert_eq!(xml.matches("<Style>").count(), 2);
        assert!(xml.contains("<Name>main</Name>"));
        assert!(xml.contains("<Name>minor</Name>"));
        assert!(xml.contains("<Title>Main</Title>"));
        assert!(xml.contains("<Title>Minor</Title>"));
        assert!(xml.contains("rule=main"));
        assert!(xml.contains("rule=minor"));
        assert!(xml.contains("version=1.1.1"));
        assert!(!xml.contains("<Name>default</Name>"));
    }

    #[test]
    fn classless_layer_keeps_default_style_111() {
        let cfg = minimal_cfg();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert_eq!(xml.matches("<Style>").count(), 1);
        assert!(xml.contains("<Name>default</Name>"));
        assert!(xml.contains("<Title>Default style</Title>"));
        assert!(!xml.contains("rule="));
    }

    #[test]
    fn class_title_falls_back_to_name_111() {
        let mut cfg = cfg_with_classes();
        cfg.layers[0].classes[0].title.clear();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Name>main</Name>"));
        assert!(xml.contains("<Title>main</Title>"));
    }

    #[test]
    fn empty_class_name_synthesises_stable_identifier_111() {
        let mut cfg = cfg_with_classes();
        cfg.layers[0].classes[0].name.clear();
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Name>class-0</Name>"));
        assert!(xml.contains("rule=class-0"));
    }

    #[test]
    fn layer_with_getmap_denied_emits_metadata_without_name_111() {
        let mut cfg = minimal_cfg();
        cfg.layers[0]
            .ows
            .request_gating
            .insert(mars_config::ServiceOp::WmsGetMap, false);
        let m = Manifest::empty(1, cfg.service.name.clone());
        let xml = capabilities_xml(&cfg, &m).unwrap();
        assert!(xml.contains("<Title>A layer</Title>"));
        assert!(!xml.contains("<Name>a</Name>"));
    }
}
