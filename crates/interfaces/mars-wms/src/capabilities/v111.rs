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
mod tests;
