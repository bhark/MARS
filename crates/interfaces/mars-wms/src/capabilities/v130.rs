//! WMS 1.3.0 GetCapabilities document.

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

/// Render the WMS 1.3.0 capabilities XML.
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

    let mut root = BytesStart::new("WMS_Capabilities");
    root.push_attribute(("version", "1.3.0"));
    root.push_attribute(("xmlns", "http://www.opengis.net/wms"));
    w.write_event(Event::Start(root)).map_err(xml_err)?;

    // service block; spec ordering: Name, Title, Abstract, KeywordList,
    // OnlineResource, ContactInformation, Fees, AccessConstraints.
    w.write_event(Event::Start(BytesStart::new("Service")))
        .map_err(xml_err)?;
    text_element(&mut w, "Name", "WMS")?;
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

    // request
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
    // GetCapabilities advertises text/xml plus the configured GetMap list;
    // historically MARS emitted only the latter. Keep the GetMap-derived
    // shape so existing tests stay green when no override is configured.
    write_request_op(&mut w, "GetCapabilities", &getmap_formats, online_href)?;
    write_request_op(&mut w, "GetMap", &getmap_formats, online_href)?;
    // gfi advertised when at least one layer permits the op; emitting it
    // unconditionally would invite identify clicks that always come back empty.
    if cfg
        .layers
        .iter()
        .any(|l| l.permits_op(mars_config::ServiceOp::WmsGetFeatureInfo))
    {
        write_request_op(&mut w, "GetFeatureInfo", &getfi_formats, online_href)?;
    }
    // legend graphic is always available; the runtime can compose a default
    // swatch even for class-less layers.
    write_request_op(&mut w, "GetLegendGraphic", &getlegend_formats, online_href)?;
    w.write_event(Event::End(BytesEnd::new("Request"))).map_err(xml_err)?;

    // root layer
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(&mut w, "Title", &cfg.service.title)?;

    // canonical-only crs advertisement: bbox values are emitted without a
    // per-crs transform, so listing every allowlist entry would lie about
    // what's available. once BBOX round-trips through the reprojection
    // allowlist the full set can be advertised again. multi-source configs
    // expand the root-layer CRS set to every distinct native crs declared
    // across cfg.sources.
    for crs in super::distinct_native_crses(cfg) {
        text_element(&mut w, "CRS", crs)?;
    }
    let root_crs = super::service_root_native_crs(cfg);
    for crs in &cfg.service.wms.advertised_crs {
        if super::distinct_native_crses(cfg).contains(&crs.as_str()) {
            continue;
        }
        text_element(&mut w, "CRS", crs)?;
    }

    if let Some(bb) = root_bbox {
        write_bbox(&mut w, root_crs, bb)?;
    }

    // root-layer authority + identifier references. 1.3.0-only - 1.1.1
    // moves identifiers under per-layer Identifier elements without the
    // authority registry.
    for auth in &cfg.service.wms.authorities {
        write_authority_url(&mut w, &auth.name, &auth.href)?;
    }
    for ident in &cfg.service.wms.identifiers {
        write_identifier(&mut w, &ident.authority, &ident.value)?;
    }

    // child layers: walk the path-derived tree so configured `group` values
    // surface as nested <Layer> elements with shared root CRS/BoundingBox.
    let tree = build_layer_tree(&cfg.layers);
    emit_children(&mut w, &tree, cfg, &layer_bboxes, &advertised_formats)?;

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

/// Emit a single `<{op}>` request block with its Format list and optional
/// `<DCPType><HTTP><Get>` advertisement.
fn write_request_op<W: std::io::Write>(
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

/// `<AuthorityURL name="..."><OnlineResource .../></AuthorityURL>` for the
/// root-layer scope. The pair surfaces from `service.wms.authorities`.
fn write_authority_url<W: std::io::Write>(w: &mut Writer<W>, name: &str, href: &str) -> Result<(), WmsError> {
    let mut au = BytesStart::new("AuthorityURL");
    au.push_attribute(("name", name));
    w.write_event(Event::Start(au)).map_err(xml_err)?;
    write_online_resource(w, href)?;
    w.write_event(Event::End(BytesEnd::new("AuthorityURL")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_identifier<W: std::io::Write>(w: &mut Writer<W>, authority: &str, value: &str) -> Result<(), WmsError> {
    let mut id = BytesStart::new("Identifier");
    id.push_attribute(("authority", authority));
    w.write_event(Event::Start(id)).map_err(xml_err)?;
    w.write_event(Event::Text(quick_xml::events::BytesText::new(value)))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("Identifier")))
        .map_err(xml_err)?;
    Ok(())
}

/// Emit each child of a node: synthesised group children first (sorted
/// by segment), real layer leaves second (config order). Mirrors the
/// stable iteration shape in [`LayerNode`]. The root layer's CRS and
/// BoundingBox are inherited per WMS 1.3.0 spec, so interior group
/// elements emit only a Title.
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
            // explicit ows.request_gating=false hides the layer
            // entirely; permits_op covers the default-allow case.
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
    // spec §7.2.4.6.3: <Name> marks a layer as GetMap-addressable. When
    // GetMap is denied the layer becomes a metadata-only container and the
    // Name must be omitted.
    if layer.permits_op(mars_config::ServiceOp::WmsGetMap) {
        text_element(w, "Name", layer.name.as_str())?;
    }
    text_element(w, "Title", &layer.title)?;
    if !layer.abstract_.is_empty() {
        text_element(w, "Abstract", &layer.abstract_)?;
    }
    write_keyword_list(w, &layer.ows.keywords)?;
    // per-layer advertised CRS override; absent = inherit from root layer.
    if let Some(crses) = layer.wms.advertised_crs.as_ref() {
        for crs in crses {
            text_element(w, "CRS", crs)?;
        }
    }
    let bbox = layer_bboxes.get(&layer.name).copied().or(layer.bbox);
    if let Some(bb) = bbox {
        write_bbox(w, super::layer_native_crs(cfg, layer), bb)?;
    }
    if let Some(attr) = layer.ows.attribution.as_ref() {
        write_attribution(w, attr)?;
    }
    for auth in &layer.ows.authorities {
        write_authority_url(w, &auth.name, &auth.href)?;
    }
    for ident in &layer.ows.identifiers {
        write_identifier(w, &ident.authority, &ident.value)?;
    }
    for mu in &layer.ows.metadata_urls {
        write_metadata_url(w, mu)?;
    }
    if let Some(scale) = &layer.scale {
        if let Some(min) = scale.min {
            text_element(w, "MinScaleDenominator", &min.to_string())?;
        }
        if let Some(max) = scale.max {
            text_element(w, "MaxScaleDenominator", &max.to_string())?;
        }
    }
    write_styles_with_legend_urls(w, layer, formats)?;
    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    Ok(())
}

fn write_attribution<W: std::io::Write>(w: &mut Writer<W>, attr: &mars_config::Attribution) -> Result<(), WmsError> {
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
        if let Some(w_) = logo.width {
            lu.push_attribute(("width", w_.to_string().as_str()));
        }
        if let Some(h) = logo.height {
            lu.push_attribute(("height", h.to_string().as_str()));
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

fn write_metadata_url<W: std::io::Write>(w: &mut Writer<W>, mu: &mars_config::MetadataUrl) -> Result<(), WmsError> {
    let mut tag = BytesStart::new("MetadataURL");
    tag.push_attribute(("type", mu.type_.as_str()));
    w.write_event(Event::Start(tag)).map_err(xml_err)?;
    text_element(w, "Format", &mu.format)?;
    write_online_resource(w, &mu.href)?;
    w.write_event(Event::End(BytesEnd::new("MetadataURL")))
        .map_err(xml_err)?;
    Ok(())
}

/// Emit one `<Style>` per configured class (or a single "default" when the
/// layer has no classes) with a LegendURL per advertised format. RULE=
/// pins the LegendURL to the same class the Style names so client legends
/// stay class-by-class consistent with the runtime renderer.
fn write_styles_with_legend_urls<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    formats: &[ImageFormat],
) -> Result<(), WmsError> {
    for ad in style_advertisements(layer) {
        write_style_block(w, layer, &ad, formats, "1.3.0")?;
    }
    Ok(())
}

fn write_style_block<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &mars_config::Layer,
    ad: &StyleAd,
    formats: &[ImageFormat],
    version: &str,
) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("Style"))).map_err(xml_err)?;
    text_element(w, "Name", &ad.name)?;
    text_element(w, "Title", &ad.title)?;
    // ~20 px default mirrors MapServer; one LegendURL per advertised format.
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
            "?service=WMS&version={}&request=GetLegendGraphic&layer={}&format={}",
            version,
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
mod tests;
