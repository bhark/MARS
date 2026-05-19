#![allow(clippy::unwrap_used)]

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
interfaces: {}
reprojection:
  allowlist: [EPSG:25832, EPSG:4326]
layers:
  - name: a
    title: "A layer"
    type: polygon
    sources:
      - { kind: postgis_table, from: t, geometry_column: g }
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
interfaces:
  wms:
    enabled: true
    formats: ["image/png", "image/jpeg", "image/webp"]
reprojection:
  allowlist: [EPSG:25832]
layers:
  - { name: a, title: A, type: polygon, sources: [{ kind: postgis_table, from: t, geometry_column: g }] }
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
sources:
  - { id: default, type: postgis, dsn: "postgres://x", native_crs: EPSG:25832 }
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp/c, max_size: 1GiB }
scales:
  bands: [{ name: hi, max_denom_exclusive: 25000 }]
interfaces: {}
reprojection:
  allowlist: [EPSG:25832]
layers:
  - name: roads
    title: "Roads"
    type: polygon
    sources: [{ kind: postgis_table, from: t, geometry_column: g }]
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
