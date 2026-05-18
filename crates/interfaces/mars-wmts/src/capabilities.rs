//! WMTS 1.0.0 GetCapabilities document.
//!
//! The envelope is OWS Common 1.1 (xmlns `http://www.opengis.net/wmts/1.0`)
//! rather than the WMS shape. The Contents block lists each advertised layer
//! and tile-matrix set; matrix dimensions come from the level config.
//!
//! Like the WMS builder, per-layer bboxes union the materialised cells in
//! the manifest, falling back to each layer's configured `bbox` when no
//! artifacts are present yet.

use std::collections::HashMap;
use std::io::Cursor;

use mars_config::{Config, ContactInfo, TileMatrixSet};
use mars_ows_common::{text_element, xml_err};
use mars_types::{Bbox, ImageFormat, LayerId, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use crate::WmtsError;

/// Render the WMTS capabilities XML.
pub fn capabilities_xml(cfg: &Config, manifest: &Manifest) -> Result<String, WmtsError> {
    let layer_bboxes = derive_layer_bboxes(cfg, manifest);
    let exposed_tms = exposed_tile_matrix_sets(cfg);

    let mut buf = Cursor::new(Vec::new());
    let mut w = Writer::new(&mut buf);

    w.write_event(Event::Decl(BytesDecl::new(
        "1.0",
        Some(cfg.service.ows.xml_encoding()),
        None,
    )))
    .map_err(xml_err)?;

    let mut root = BytesStart::new("Capabilities");
    root.push_attribute(("xmlns", "http://www.opengis.net/wmts/1.0"));
    root.push_attribute(("xmlns:ows", "http://www.opengis.net/ows/1.1"));
    root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    root.push_attribute(("version", "1.0.0"));
    w.write_event(Event::Start(root)).map_err(xml_err)?;

    write_service_identification(&mut w, cfg)?;
    write_service_provider(&mut w, cfg)?;
    write_operations_metadata(&mut w, cfg)?;

    // contents
    w.write_event(Event::Start(BytesStart::new("Contents")))
        .map_err(xml_err)?;
    let advertised_formats = configured_formats(cfg);
    for layer in &cfg.layers {
        // explicit ows.request_gating denial hides the layer entirely.
        // permits_op covers the default-allow case.
        if !layer.permits_op(mars_config::ServiceOp::WmtsGetTile) {
            continue;
        }
        write_layer(
            &mut w,
            cfg,
            layer,
            layer_bboxes.get(&layer.name).copied(),
            &exposed_tms,
            &advertised_formats,
        )?;
    }
    for (name, tms) in &exposed_tms {
        write_tile_matrix_set(&mut w, name, tms)?;
    }
    w.write_event(Event::End(BytesEnd::new("Contents"))).map_err(xml_err)?;

    w.write_event(Event::End(BytesEnd::new("Capabilities")))
        .map_err(xml_err)?;

    String::from_utf8(buf.into_inner()).map_err(|e| WmtsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    })
}

fn write_service_identification<W: std::io::Write>(w: &mut Writer<W>, cfg: &Config) -> Result<(), WmtsError> {
    w.write_event(Event::Start(BytesStart::new("ows:ServiceIdentification")))
        .map_err(xml_err)?;
    text_element(w, "ows:Title", &cfg.service.title)?;
    if !cfg.service.abstract_.is_empty() {
        text_element(w, "ows:Abstract", &cfg.service.abstract_)?;
    }
    if !cfg.service.ows.keywords.is_empty() {
        w.write_event(Event::Start(BytesStart::new("ows:Keywords")))
            .map_err(xml_err)?;
        for kw in &cfg.service.ows.keywords {
            text_element(w, "ows:Keyword", kw)?;
        }
        w.write_event(Event::End(BytesEnd::new("ows:Keywords")))
            .map_err(xml_err)?;
    }
    text_element(w, "ows:ServiceType", "OGC WMTS")?;
    text_element(w, "ows:ServiceTypeVersion", "1.0.0")?;
    if let Some(fees) = cfg.service.ows.fees.as_deref() {
        text_element(w, "ows:Fees", fees)?;
    }
    if let Some(ac) = cfg.service.ows.access_constraints.as_deref() {
        text_element(w, "ows:AccessConstraints", ac)?;
    }
    w.write_event(Event::End(BytesEnd::new("ows:ServiceIdentification")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_service_provider<W: std::io::Write>(w: &mut Writer<W>, cfg: &Config) -> Result<(), WmtsError> {
    let contact = &cfg.service.contact;
    let email = if !contact.email.is_empty() {
        contact.email.as_str()
    } else {
        cfg.service.contact_email.as_str()
    };
    if contact.is_empty() && email.is_empty() {
        return Ok(());
    }
    let provider_name = if !contact.organization.is_empty() {
        contact.organization.as_str()
    } else {
        cfg.service.title.as_str()
    };
    w.write_event(Event::Start(BytesStart::new("ows:ServiceProvider")))
        .map_err(xml_err)?;
    text_element(w, "ows:ProviderName", provider_name)?;
    if let Some(href) = cfg.service.ows.online_resource.as_deref() {
        let mut ps = BytesStart::new("ows:ProviderSite");
        ps.push_attribute(("xlink:href", href));
        w.write_event(Event::Empty(ps)).map_err(xml_err)?;
    }
    write_service_contact(w, contact, email, cfg.service.ows.online_resource.as_deref())?;
    w.write_event(Event::End(BytesEnd::new("ows:ServiceProvider")))
        .map_err(xml_err)?;
    Ok(())
}

/// OWS Common 1.1 ServiceContact emit. Sub-elements that are empty are
/// dropped so the document stays clean when only partial contact data is
/// configured.
fn write_service_contact<W: std::io::Write>(
    w: &mut Writer<W>,
    contact: &ContactInfo,
    fallback_email: &str,
    online_resource: Option<&str>,
) -> Result<(), WmtsError> {
    w.write_event(Event::Start(BytesStart::new("ows:ServiceContact")))
        .map_err(xml_err)?;
    if !contact.person.is_empty() {
        text_element(w, "ows:IndividualName", &contact.person)?;
    }
    if !contact.position.is_empty() {
        text_element(w, "ows:PositionName", &contact.position)?;
    }
    let need_contact_info = !contact.phone.is_empty()
        || !contact.fax.is_empty()
        || !contact.address.is_empty()
        || !fallback_email.is_empty()
        || online_resource.is_some();
    if need_contact_info {
        w.write_event(Event::Start(BytesStart::new("ows:ContactInfo")))
            .map_err(xml_err)?;
        if !contact.phone.is_empty() || !contact.fax.is_empty() {
            w.write_event(Event::Start(BytesStart::new("ows:Phone")))
                .map_err(xml_err)?;
            if !contact.phone.is_empty() {
                text_element(w, "ows:Voice", &contact.phone)?;
            }
            if !contact.fax.is_empty() {
                text_element(w, "ows:Facsimile", &contact.fax)?;
            }
            w.write_event(Event::End(BytesEnd::new("ows:Phone"))).map_err(xml_err)?;
        }
        if !contact.address.is_empty() || !fallback_email.is_empty() {
            let a = &contact.address;
            w.write_event(Event::Start(BytesStart::new("ows:Address")))
                .map_err(xml_err)?;
            if !a.street.is_empty() {
                text_element(w, "ows:DeliveryPoint", &a.street)?;
            }
            if !a.city.is_empty() {
                text_element(w, "ows:City", &a.city)?;
            }
            if !a.state_or_province.is_empty() {
                text_element(w, "ows:AdministrativeArea", &a.state_or_province)?;
            }
            if !a.postcode.is_empty() {
                text_element(w, "ows:PostalCode", &a.postcode)?;
            }
            if !a.country.is_empty() {
                text_element(w, "ows:Country", &a.country)?;
            }
            if !fallback_email.is_empty() {
                text_element(w, "ows:ElectronicMailAddress", fallback_email)?;
            }
            w.write_event(Event::End(BytesEnd::new("ows:Address")))
                .map_err(xml_err)?;
        }
        if let Some(href) = online_resource {
            let mut or = BytesStart::new("ows:OnlineResource");
            or.push_attribute(("xlink:href", href));
            w.write_event(Event::Empty(or)).map_err(xml_err)?;
        }
        w.write_event(Event::End(BytesEnd::new("ows:ContactInfo")))
            .map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("ows:ServiceContact")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_operations_metadata<W: std::io::Write>(w: &mut Writer<W>, cfg: &Config) -> Result<(), WmtsError> {
    // when an online resource is configured, advertise the canonical
    // DCP/HTTP/Get binding pointing at it. otherwise emit empty Operation
    // elements - clients fall back to the request URL they reached the
    // service on, which is the common WMTS deployment practice.
    let online_href = cfg.service.ows.online_resource.as_deref();
    w.write_event(Event::Start(BytesStart::new("ows:OperationsMetadata")))
        .map_err(xml_err)?;
    for op in ["GetCapabilities", "GetTile"] {
        let mut o = BytesStart::new("ows:Operation");
        o.push_attribute(("name", op));
        w.write_event(Event::Start(o)).map_err(xml_err)?;
        if let Some(href) = online_href {
            w.write_event(Event::Start(BytesStart::new("ows:DCP")))
                .map_err(xml_err)?;
            w.write_event(Event::Start(BytesStart::new("ows:HTTP")))
                .map_err(xml_err)?;
            let mut get = BytesStart::new("ows:Get");
            get.push_attribute(("xlink:href", href));
            w.write_event(Event::Empty(get)).map_err(xml_err)?;
            w.write_event(Event::End(BytesEnd::new("ows:HTTP"))).map_err(xml_err)?;
            w.write_event(Event::End(BytesEnd::new("ows:DCP"))).map_err(xml_err)?;
        }
        w.write_event(Event::End(BytesEnd::new("ows:Operation")))
            .map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("ows:OperationsMetadata")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_layer<W: std::io::Write>(
    w: &mut Writer<W>,
    cfg: &Config,
    layer: &mars_config::Layer,
    bbox: Option<Bbox>,
    exposed_tms: &[(String, TileMatrixSet)],
    formats: &[ImageFormat],
) -> Result<(), WmtsError> {
    w.write_event(Event::Start(BytesStart::new("Layer"))).map_err(xml_err)?;
    text_element(w, "ows:Title", &layer.title)?;
    if !layer.abstract_.is_empty() {
        text_element(w, "ows:Abstract", &layer.abstract_)?;
    }
    if let Some(bb) = bbox.or(layer.bbox) {
        write_bbox(w, layer_native_crs(cfg, layer), bb)?;
    }
    text_element(w, "ows:Identifier", layer.name.as_str())?;

    // a single default style. SLD is a non-goal; we expose only
    // the compiled-in default to keep the document honest.
    w.write_event({
        let mut s = BytesStart::new("Style");
        s.push_attribute(("isDefault", "true"));
        Event::Start(s)
    })
    .map_err(xml_err)?;
    text_element(w, "ows:Identifier", "default")?;
    w.write_event(Event::End(BytesEnd::new("Style"))).map_err(xml_err)?;

    for fmt in formats {
        text_element(w, "Format", fmt.mime())?;
    }

    for (name, _) in exposed_tms {
        w.write_event(Event::Start(BytesStart::new("TileMatrixSetLink")))
            .map_err(xml_err)?;
        text_element(w, "TileMatrixSet", name)?;
        w.write_event(Event::End(BytesEnd::new("TileMatrixSetLink")))
            .map_err(xml_err)?;
    }

    // advertise the rest tile template so clients that prefer the resource
    // URL form discover it without out-of-band knowledge. relative path
    // matches the operations-metadata convention: clients ground it against
    // the request URL they reached the service on.
    for fmt in formats {
        let mut r = BytesStart::new("ResourceURL");
        r.push_attribute(("format", fmt.mime()));
        r.push_attribute(("resourceType", "tile"));
        r.push_attribute((
            "template",
            format!(
                "/wmts/{}/default/{{TileMatrixSet}}/{{TileMatrix}}/{{TileRow}}/{{TileCol}}.{}",
                layer.name.as_str(),
                rest_ext_for(*fmt)
            )
            .as_str(),
        ));
        w.write_event(Event::Empty(r)).map_err(xml_err)?;
    }

    w.write_event(Event::End(BytesEnd::new("Layer"))).map_err(xml_err)?;
    Ok(())
}

fn rest_ext_for(fmt: ImageFormat) -> &'static str {
    match fmt {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
    }
}

fn write_tile_matrix_set<W: std::io::Write>(
    w: &mut Writer<W>,
    name: &str,
    tms: &TileMatrixSet,
) -> Result<(), WmtsError> {
    w.write_event(Event::Start(BytesStart::new("TileMatrixSet")))
        .map_err(xml_err)?;
    text_element(w, "ows:Identifier", name)?;
    text_element(w, "ows:SupportedCRS", tms.crs.as_str())?;
    for level in &tms.levels {
        w.write_event(Event::Start(BytesStart::new("TileMatrix")))
            .map_err(xml_err)?;
        text_element(w, "ows:Identifier", &level.id.to_string())?;
        text_element(w, "ScaleDenominator", &level.scale_denominator.to_string())?;
        text_element(w, "TopLeftCorner", &format!("{} {}", tms.top_left[0], tms.top_left[1]))?;
        text_element(w, "TileWidth", &tms.tile_size[0].to_string())?;
        text_element(w, "TileHeight", &tms.tile_size[1].to_string())?;
        text_element(w, "MatrixWidth", &level.matrix_width.to_string())?;
        text_element(w, "MatrixHeight", &level.matrix_height.to_string())?;
        w.write_event(Event::End(BytesEnd::new("TileMatrix")))
            .map_err(xml_err)?;
    }
    w.write_event(Event::End(BytesEnd::new("TileMatrixSet")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_bbox<W: std::io::Write>(w: &mut Writer<W>, crs: &str, bbox: Bbox) -> Result<(), WmtsError> {
    let mut bb = BytesStart::new("ows:BoundingBox");
    bb.push_attribute(("crs", crs));
    w.write_event(Event::Start(bb)).map_err(xml_err)?;
    text_element(w, "ows:LowerCorner", &format!("{} {}", bbox.min_x, bbox.min_y))?;
    text_element(w, "ows:UpperCorner", &format!("{} {}", bbox.max_x, bbox.max_y))?;
    w.write_event(Event::End(BytesEnd::new("ows:BoundingBox")))
        .map_err(xml_err)?;
    Ok(())
}

/// Resolve a layer's native CRS for bbox labelling. Raster layers take
/// `raster.source.source_crs`; vector layers take the CRS of the source
/// feeding their first binding. Falls back to the first configured source.
fn layer_native_crs<'a>(cfg: &'a Config, layer: &'a mars_config::Layer) -> &'a str {
    if let Some(raster) = layer.raster.as_ref() {
        return raster.source.source_crs.as_str();
    }
    if let Some(first) = layer.sources.first()
        && let Some(src) = cfg.sources.iter().find(|s| s.id == first.source)
    {
        return src.native_crs.as_str();
    }
    cfg.sources.first().map(|s| s.native_crs.as_str()).unwrap_or("")
}

/// Resolve the tile-matrix-sets to advertise. Honours the
/// `interfaces.wmts.tile_matrix_sets` allowlist if set; otherwise advertises
/// every set defined in `tile_matrix_sets`. Output is a Vec to keep emit
/// order deterministic (BTreeMap iteration is already sorted).
fn exposed_tile_matrix_sets(cfg: &Config) -> Vec<(String, TileMatrixSet)> {
    let allow: Option<Vec<&str>> = cfg
        .interfaces
        .wmts
        .as_ref()
        .map(|w| w.tile_matrix_sets.iter().map(String::as_str).collect());
    cfg.tile_matrix_sets
        .iter()
        .filter(|(name, _)| match &allow {
            Some(names) if !names.is_empty() => names.contains(&name.as_str()),
            _ => true,
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn configured_formats(cfg: &Config) -> Vec<ImageFormat> {
    let configured = cfg
        .interfaces
        .wmts
        .as_ref()
        .map(|w| w.formats.as_slice())
        .unwrap_or(&[]);
    mars_ows_common::configured_formats(configured, ImageFormat::Png)
}

fn derive_layer_bboxes(cfg: &Config, _manifest: &Manifest) -> HashMap<LayerId, Bbox> {
    // phase-b: cell-walk replaced by config-only fallback. phase-d will union
    // per-binding `combined_bbox` summaries from the v3 page entries.
    let mut out: HashMap<LayerId, Bbox> = HashMap::new();
    for layer in &cfg.layers {
        if let Some(bbox) = layer.bbox {
            out.entry(layer.name.clone()).or_insert(bbox);
        }
    }
    out
}

#[cfg(test)]
mod tests;
