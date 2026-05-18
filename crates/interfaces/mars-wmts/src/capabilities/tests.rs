#![allow(clippy::unwrap_used)]

use super::*;
use quick_xml::Reader;

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
      - { kind: postgis_table, from: t, geometry_column: g }
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

#[test]
fn layer_with_wmts_get_tile_denied_is_hidden() {
    let mut cfg = minimal_cfg();
    cfg.layers[0]
        .ows
        .request_gating
        .insert(mars_config::ServiceOp::WmtsGetTile, false);
    let xml = capabilities_xml(&cfg, &empty_manifest(&cfg)).unwrap();
    assert!(
        !xml.contains("<ows:Identifier>a</ows:Identifier>"),
        "denied layer must not surface in Contents",
    );
}

// phase-d: re-add `derives_layer_bbox_from_manifest_cells` once v3 page
// entries surface per-binding bboxes the wmts builder can union.
