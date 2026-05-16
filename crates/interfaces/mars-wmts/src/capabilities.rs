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
use mars_types::{Bbox, ImageFormat, LayerId, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};

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

fn text_element<W: std::io::Write>(w: &mut Writer<W>, name: &str, text: &str) -> Result<(), WmtsError> {
    w.write_event(Event::Start(BytesStart::new(name))).map_err(xml_err)?;
    w.write_event(Event::Text(BytesText::new(text))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new(name))).map_err(xml_err)?;
    Ok(())
}

fn xml_err(e: std::io::Error) -> WmtsError {
    WmtsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    }
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
    let configured: Vec<ImageFormat> = cfg
        .interfaces
        .wmts
        .as_ref()
        .map(|w| {
            w.formats
                .iter()
                .filter_map(|f| ImageFormat::from_mime(f.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec![ImageFormat::Png]
    } else {
        configured
    }
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quick_xml::Reader;

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
interfaces:
  wmts:
    enabled: true
    tile_matrix_sets: [dk_25832]
    formats: [image/png]
tile_matrix_sets:
  dk_25832:
    crs: EPSG:25832
    top_left: [120000, 6500000]
    tile_size: [256, 256]
    levels:
      - { id: 0, scale_denominator: 25000000, matrix_width: 1, matrix_height: 1 }
      - { id: 1, scale_denominator: 12500000, matrix_width: 2, matrix_height: 2 }
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: a
    title: "A layer"
    type: polygon
    sources:
      - { from: t, geometry_column: g }
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn empty_manifest(cfg: &Config) -> Manifest {
        Manifest::empty(1, cfg.service.name.clone())
    }

    fn parses_balanced(xml: &str) {
        let mut r = Reader::from_str(xml);
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
    fn happy_path() {
        let cfg = minimal_cfg();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        parses_balanced(&xml);
        assert!(xml.contains("<Capabilities"));
        assert!(xml.contains(r#"version="1.0.0""#));
        assert!(xml.contains("OGC WMTS"));
        assert!(xml.contains("<ows:Identifier>a</ows:Identifier>"));
        assert!(xml.contains("<TileMatrixSet>"));
        assert!(xml.contains("<ows:Identifier>dk_25832</ows:Identifier>"));
        assert!(xml.contains("<MatrixWidth>1</MatrixWidth>"));
        assert!(xml.contains("<MatrixHeight>2</MatrixHeight>"));
        assert!(xml.contains("<TileWidth>256</TileWidth>"));
        assert!(xml.contains("<ScaleDenominator>25000000</ScaleDenominator>"));
        assert!(xml.contains("<Format>image/png</Format>"));
    }

    #[test]
    fn uses_ows_envelope_not_wms() {
        let cfg = minimal_cfg();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        // strict WMTS clients reject WMS-shaped roots
        assert!(!xml.contains("WMS_Capabilities"));
        assert!(xml.contains("http://www.opengis.net/wmts/1.0"));
        assert!(xml.contains("http://www.opengis.net/ows/1.1"));
    }

    #[test]
    fn escapes_xml_special_chars() {
        let mut cfg = minimal_cfg();
        cfg.layers[0].title = "A & B <C>".into();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(!xml.contains("A & B <C>"));
        assert!(xml.contains("A &amp; B &lt;C&gt;"));
    }

    #[test]
    fn omits_service_provider_when_email_empty() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(!xml.contains("ServiceProvider"));
    }

    #[test]
    fn tms_allowlist_filters_set() {
        // a second matrix set defined but not advertised
        let mut cfg = minimal_cfg();
        cfg.tile_matrix_sets.insert(
            "extra".to_owned(),
            cfg.tile_matrix_sets.get("dk_25832").cloned().unwrap(),
        );
        // `interfaces.wmts.tile_matrix_sets: [dk_25832]` already restricts to one
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("dk_25832"));
        assert!(!xml.contains("<ows:Identifier>extra</ows:Identifier>"));
    }

    #[test]
    fn service_identification_emits_keywords_fees_access_constraints() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.keywords = vec!["tiles".into(), "raster".into()];
        cfg.service.ows.fees = Some("none".into());
        cfg.service.ows.access_constraints = Some("CC-BY 4.0".into());
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ows:Keywords>"));
        assert!(xml.contains("<ows:Keyword>tiles</ows:Keyword>"));
        assert!(xml.contains("<ows:Keyword>raster</ows:Keyword>"));
        assert!(xml.contains("<ows:Fees>none</ows:Fees>"));
        assert!(xml.contains("<ows:AccessConstraints>CC-BY 4.0</ows:AccessConstraints>"));
    }

    #[test]
    fn service_provider_uses_organization_when_set() {
        let mut cfg = minimal_cfg();
        cfg.service.contact = mars_config::ContactInfo {
            organization: "Acme Maps".into(),
            email: "ops@acme".into(),
            ..Default::default()
        };
        cfg.service.contact_email = String::new();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ows:ProviderName>Acme Maps</ows:ProviderName>"));
        assert!(xml.contains("<ows:ElectronicMailAddress>ops@acme</ows:ElectronicMailAddress>"));
    }

    #[test]
    fn service_provider_falls_back_to_title_when_organization_empty() {
        let cfg = minimal_cfg();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ows:ProviderName>T</ows:ProviderName>"));
    }

    #[test]
    fn provider_site_emitted_when_online_resource_set() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.online_resource = Some("https://wmts.example/?".into());
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains(r#"<ows:ProviderSite xlink:href="https://wmts.example/?""#));
    }

    #[test]
    fn full_contact_emits_ows_shape() {
        let mut cfg = minimal_cfg();
        cfg.service.contact_email = String::new();
        cfg.service.contact = mars_config::ContactInfo {
            person: "Pat".into(),
            position: "Lead".into(),
            organization: "Acme".into(),
            phone: "+1-555-0100".into(),
            fax: "+1-555-0101".into(),
            email: "p@acme".into(),
            address: mars_config::Address {
                street: "1 Main".into(),
                city: "Springfield".into(),
                state_or_province: "IL".into(),
                postcode: "62701".into(),
                country: "US".into(),
                ..Default::default()
            },
        };
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ows:IndividualName>Pat</ows:IndividualName>"));
        assert!(xml.contains("<ows:PositionName>Lead</ows:PositionName>"));
        assert!(xml.contains("<ows:Voice>+1-555-0100</ows:Voice>"));
        assert!(xml.contains("<ows:Facsimile>+1-555-0101</ows:Facsimile>"));
        assert!(xml.contains("<ows:DeliveryPoint>1 Main</ows:DeliveryPoint>"));
        assert!(xml.contains("<ows:City>Springfield</ows:City>"));
        assert!(xml.contains("<ows:AdministrativeArea>IL</ows:AdministrativeArea>"));
        assert!(xml.contains("<ows:PostalCode>62701</ows:PostalCode>"));
        assert!(xml.contains("<ows:Country>US</ows:Country>"));
        assert!(xml.contains("<ows:ElectronicMailAddress>p@acme</ows:ElectronicMailAddress>"));
    }

    #[test]
    fn operations_metadata_emits_dcp_when_online_resource_set() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.online_resource = Some("https://wmts.example/?".into());
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ows:DCP>"));
        assert!(xml.contains("<ows:HTTP>"));
        assert!(xml.contains(r#"<ows:Get xlink:href="https://wmts.example/?""#));
    }

    #[test]
    fn operations_metadata_omits_dcp_when_no_online_resource() {
        let cfg = minimal_cfg();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(!xml.contains("<ows:DCP>"));
        assert!(xml.contains(r#"<ows:Operation name="GetCapabilities">"#));
    }

    #[test]
    fn xml_encoding_honored_wmts() {
        let mut cfg = minimal_cfg();
        cfg.service.ows.encoding = Some("ISO-8859-1".into());
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="ISO-8859-1""#));
    }

    #[test]
    fn emits_rest_resource_url_template() {
        let cfg = minimal_cfg();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        assert!(xml.contains("<ResourceURL"));
        assert!(xml.contains(r#"resourceType="tile""#));
        assert!(xml.contains(r#"format="image/png""#));
        assert!(xml.contains("/wmts/a/default/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.png"));
    }

    #[test]
    fn empty_layers_is_valid() {
        let mut cfg = minimal_cfg();
        cfg.layers.clear();
        let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
        parses_balanced(&xml);
        assert!(xml.contains("<Contents>"));
        assert!(xml.contains("</Contents>"));
    }

    // phase-d: re-add `derives_layer_bbox_from_manifest_cells` once v3 page
    // entries surface per-binding bboxes the wmts builder can union.
}
