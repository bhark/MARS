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
      - { kind: postgis_table, from: t, geometry_column: g }
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
    cfg.layers[0].wms.enable_get_feature_info = true;
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

fn count_event<F: Fn(&[u8]) -> bool>(xml: &str, want_start: bool, pred: F) -> usize {
    let mut r = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut n = 0;
    loop {
        match r.read_event_into(&mut buf).unwrap() {
            Event::Start(e) if want_start && pred(e.name().as_ref()) => n += 1,
            Event::End(e) if !want_start && pred(e.name().as_ref()) => n += 1,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    n
}

fn extract_titles(xml: &str) -> Vec<String> {
    let mut r = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut titles = Vec::new();
    let mut in_title = false;
    loop {
        match r.read_event_into(&mut buf).unwrap() {
            Event::Start(e) if e.name().as_ref() == b"Title" => in_title = true,
            Event::End(e) if e.name().as_ref() == b"Title" => in_title = false,
            Event::Text(t) if in_title => titles.push(String::from_utf8_lossy(&t.into_inner()).to_string()),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    titles
}

fn cfg_with_groups() -> Config {
    let yaml = r#"
service: { name: t, title: "Root", abstract: "A", contact_email: "" }
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
  allowlist: [EPSG:25832]
layers:
  - { name: ungrouped, title: "Ungrouped", type: polygon, sources: [{ kind: postgis_table, from: u, geometry_column: g }] }
  - { name: basemap, title: "Basemap", type: polygon, group: "/Basis", sources: [{ kind: postgis_table, from: b, geometry_column: g }] }
  - { name: bygning, title: "Bygning", type: polygon, group: "/Adresse/Bygning", sources: [{ kind: postgis_table, from: y, geometry_column: g }] }
  - { name: park, title: "Park", type: polygon, group: "/Basis", sources: [{ kind: postgis_table, from: p, geometry_column: g }] }
"#;
    serde_yaml_ng::from_str(yaml).unwrap()
}

/// 4 leaves + 1 root + 2 distinct group paths (/Basis, /Adresse/Bygning).
/// Synthesised parents: Basis (1), Adresse (1), Adresse/Bygning (1). So
/// total <Layer> opens = root(1) + groups(3) + leaves(4) = 8.
#[test]
fn nested_groups_emit_intermediate_layer_elements() {
    let cfg = cfg_with_groups();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert_eq!(count_event(&xml, true, |n| n == b"Layer"), 8);
    assert_eq!(count_event(&xml, false, |n| n == b"Layer"), 8);
}

/// Synthesised parents use the path segment as Title. Real leaves use
/// the configured layer.title. The service root contributes its own
/// Title before any layer/group is emitted.
#[test]
fn nested_group_titles_appear_in_tree_order() {
    let cfg = cfg_with_groups();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    let titles = extract_titles(&xml);
    // service title appears in <Service><Title> AND in the root <Layer>.
    // synthesised group children come first, sorted alphabetically by
    // segment ("Adresse" before "Basis"). Real-leaf children land last
    // (ungrouped is the only direct-root leaf).
    assert!(titles.contains(&"Adresse".into()), "Adresse parent title: {titles:?}");
    assert!(titles.contains(&"Bygning".into()), "Bygning parent title: {titles:?}");
    assert!(titles.contains(&"Basis".into()), "Basis parent title: {titles:?}");
    assert!(titles.contains(&"Bygning".into()) && titles.contains(&"Basemap".into()));
    let adresse = titles.iter().position(|t| t == "Adresse").unwrap();
    let basis = titles.iter().position(|t| t == "Basis").unwrap();
    assert!(adresse < basis, "synthesised parents must sort alphabetically");
}

/// Real leaves inside groups still carry their <Name> so GetMap can
/// address them. Synthesised parents have no <Name> -> not GetMap-able.
#[test]
fn synthesised_parents_omit_name_real_leaves_keep_it() {
    let cfg = cfg_with_groups();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    for name in ["basemap", "bygning", "park", "ungrouped"] {
        assert!(
            xml.contains(&format!("<Name>{name}</Name>")),
            "leaf <Name>{name}</Name> missing"
        );
    }
    // synthesised group names ("Basis", "Adresse", "Bygning") must NOT
    // appear as <Name>. Title is fine; Name would imply GetMap addressable.
    assert!(!xml.contains("<Name>Basis</Name>"));
    assert!(!xml.contains("<Name>Adresse</Name>"));
    assert!(!xml.contains("<Name>Bygning</Name>"));
}

#[test]
fn emits_keyword_list_when_configured() {
    let mut cfg = minimal_cfg();
    cfg.service.ows.keywords = vec!["roads".into(), "buildings".into()];
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<KeywordList>"));
    assert!(xml.contains("<Keyword>roads</Keyword>"));
    assert!(xml.contains("<Keyword>buildings</Keyword>"));
}

#[test]
fn omits_keyword_list_when_empty() {
    let cfg = minimal_cfg();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(!xml.contains("<KeywordList>"));
}

#[test]
fn emits_online_resource_on_service() {
    let mut cfg = minimal_cfg();
    cfg.service.ows.online_resource = Some("https://wms.example.org/?".into());
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains(r#"xlink:href="https://wms.example.org/?""#));
}

#[test]
fn emits_fees_and_access_constraints() {
    let mut cfg = minimal_cfg();
    cfg.service.ows.fees = Some("none".into());
    cfg.service.ows.access_constraints = Some("CC-BY 4.0".into());
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Fees>none</Fees>"));
    assert!(xml.contains("<AccessConstraints>CC-BY 4.0</AccessConstraints>"));
}

#[test]
fn full_contact_block_emitted_when_set() {
    let mut cfg = minimal_cfg();
    cfg.service.contact_email = String::new();
    cfg.service.contact = mars_config::ContactInfo {
        person: "Pat Operator".into(),
        position: "Lead".into(),
        organization: "Acme".into(),
        phone: "+1-555-0100".into(),
        fax: "+1-555-0101".into(),
        email: "ops@acme.example".into(),
        address: mars_config::Address {
            street: "1 Main St".into(),
            city: "Springfield".into(),
            state_or_province: "IL".into(),
            postcode: "62701".into(),
            country: "US".into(),
            ..Default::default()
        },
    };
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<ContactInformation>"));
    assert!(xml.contains("<ContactPerson>Pat Operator</ContactPerson>"));
    assert!(xml.contains("<ContactOrganization>Acme</ContactOrganization>"));
    assert!(xml.contains("<ContactPosition>Lead</ContactPosition>"));
    assert!(xml.contains("<AddressType>postal</AddressType>"));
    assert!(xml.contains("<Address>1 Main St</Address>"));
    assert!(xml.contains("<City>Springfield</City>"));
    assert!(xml.contains("<StateOrProvince>IL</StateOrProvince>"));
    assert!(xml.contains("<PostCode>62701</PostCode>"));
    assert!(xml.contains("<Country>US</Country>"));
    assert!(xml.contains("<ContactVoiceTelephone>+1-555-0100</ContactVoiceTelephone>"));
    assert!(xml.contains("<ContactFacsimileTelephone>+1-555-0101</ContactFacsimileTelephone>"));
    assert!(xml.contains("<ContactElectronicMailAddress>ops@acme.example</ContactElectronicMailAddress>"));
}

#[test]
fn structured_contact_email_takes_precedence_over_legacy() {
    let mut cfg = minimal_cfg();
    cfg.service.contact_email = "legacy@x".into();
    cfg.service.contact.email = "new@x".into();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<ContactElectronicMailAddress>new@x</ContactElectronicMailAddress>"));
    assert!(!xml.contains("legacy@x"));
}

#[test]
fn authority_url_and_identifier_on_root_layer() {
    let mut cfg = minimal_cfg();
    cfg.service.wms.authorities = vec![mars_config::AuthorityRef {
        name: "iso19115".into(),
        href: "https://example.org/auth".into(),
    }];
    cfg.service.wms.identifiers = vec![mars_config::IdentifierRef {
        authority: "iso19115".into(),
        value: "urn:abc".into(),
    }];
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains(r#"<AuthorityURL name="iso19115">"#));
    assert!(xml.contains(r#"xlink:href="https://example.org/auth""#));
    assert!(xml.contains(r#"<Identifier authority="iso19115">urn:abc</Identifier>"#));
}

#[test]
fn xml_encoding_honored() {
    let mut cfg = minimal_cfg();
    cfg.service.ows.encoding = Some("ISO-8859-1".into());
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.starts_with(r#"<?xml version="1.0" encoding="ISO-8859-1""#));
}

#[test]
fn dcp_type_emitted_when_online_resource_set() {
    let mut cfg = minimal_cfg();
    cfg.service.ows.online_resource = Some("https://wms.example.org/?".into());
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    // DCPType under GetCapabilities, GetMap (and GetLegendGraphic) at minimum
    assert!(xml.contains("<DCPType>"));
    assert!(xml.contains("<HTTP>"));
    assert!(xml.contains("<Get>"));
}

#[test]
fn service_formats_override_request_block_lists() {
    let mut cfg = minimal_cfg();
    cfg.service.wms.formats.get_map = vec!["image/svg+xml".into(), "application/pdf".into()];
    cfg.service.wms.formats.get_feature_info = vec!["application/gml+xml".into()];
    // mark layer queryable so GetFeatureInfo block emits
    cfg.layers[0].wms.enable_get_feature_info = true;
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Format>image/svg+xml</Format>"));
    assert!(xml.contains("<Format>application/pdf</Format>"));
    assert!(xml.contains("<Format>application/gml+xml</Format>"));
}

#[test]
fn advertised_crs_adds_to_root_layer_dedup_against_native() {
    let mut cfg = minimal_cfg();
    cfg.service.wms.advertised_crs = vec!["EPSG:25832".into(), "EPSG:3857".into(), "EPSG:4326".into()];
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    // native always advertised; duplicates dropped
    assert_eq!(xml.matches("<CRS>EPSG:25832</CRS>").count(), 1);
    assert!(xml.contains("<CRS>EPSG:3857</CRS>"));
    assert!(xml.contains("<CRS>EPSG:4326</CRS>"));
}

#[test]
fn per_layer_keywords_and_metadata_url_emitted() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].ows.keywords = vec!["roads".into(), "transport".into()];
    cfg.layers[0].ows.metadata_urls = vec![mars_config::MetadataUrl {
        type_: "ISO19115:2003".into(),
        format: "text/xml".into(),
        href: "https://example.org/md/roads.xml".into(),
    }];
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Keyword>roads</Keyword>"));
    assert!(xml.contains(r#"<MetadataURL type="ISO19115:2003">"#));
    assert!(xml.contains("<Format>text/xml</Format>"));
    assert!(xml.contains("https://example.org/md/roads.xml"));
}

#[test]
fn per_layer_advertised_crs_overrides_root() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].wms.advertised_crs = Some(vec!["EPSG:3857".into(), "EPSG:4326".into()]);
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<CRS>EPSG:3857</CRS>"));
    assert!(xml.contains("<CRS>EPSG:4326</CRS>"));
}

#[test]
fn opaque_attribute_emitted_when_set() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].wms.opaque = true;
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains(r#"opaque="1""#));
}

#[test]
fn attribution_block_emitted() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].ows.attribution = Some(mars_config::Attribution {
        title: "Acme Maps".into(),
        online_resource: Some("https://acme.example".into()),
        logo: Some(mars_config::LogoUrl {
            format: "image/png".into(),
            href: "https://acme.example/logo.png".into(),
            width: Some(120),
            height: Some(80),
        }),
    });
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Attribution>"));
    assert!(xml.contains("<Title>Acme Maps</Title>"));
    assert!(xml.contains("<LogoURL"));
    assert!(xml.contains(r#"width="120""#));
    assert!(xml.contains(r#"height="80""#));
    assert!(xml.contains("<Format>image/png</Format>"));
}

#[test]
fn per_layer_authority_and_identifier_emitted() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].ows.authorities = vec![mars_config::AuthorityRef {
        name: "isri".into(),
        href: "https://example.org/isri".into(),
    }];
    cfg.layers[0].ows.identifiers = vec![mars_config::IdentifierRef {
        authority: "isri".into(),
        value: "urn:layer:roads".into(),
    }];
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains(r#"<AuthorityURL name="isri">"#));
    assert!(xml.contains(r#"<Identifier authority="isri">urn:layer:roads</Identifier>"#));
}

#[test]
fn layer_with_capabilities_denied_is_hidden() {
    let mut cfg = minimal_cfg();
    cfg.layers[0]
        .ows
        .request_gating
        .insert(mars_config::ServiceOp::WmsGetCapabilities, false);
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(!xml.contains("<Name>a</Name>"), "denied layer must not appear");
}

#[test]
fn layer_with_getmap_denied_emits_metadata_without_name() {
    let mut cfg = minimal_cfg();
    cfg.layers[0]
        .ows
        .request_gating
        .insert(mars_config::ServiceOp::WmsGetMap, false);
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    // metadata still surfaces - the layer remains in capabilities as an
    // abstract parent.
    assert!(xml.contains("<Title>A layer</Title>"));
    // <Name> must be absent so clients cannot address it via GetMap.
    assert!(!xml.contains("<Name>a</Name>"));
}

#[test]
fn gfi_gating_honors_explicit_request_gating_over_enable_flag() {
    let mut cfg = minimal_cfg();
    cfg.layers[0].wms.enable_get_feature_info = false;
    cfg.layers[0]
        .ows
        .request_gating
        .insert(mars_config::ServiceOp::WmsGetFeatureInfo, true);
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains(r#"<Layer queryable="1">"#));
    assert!(xml.contains("<GetFeatureInfo>"));
}

#[test]
fn advertises_configured_formats() {
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
  - { name: a, title: A, type: polygon, sources: [{ kind: postgis_table, from: t, geometry_column: g }] }
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Format>image/png</Format>"));
    assert!(xml.contains("<Format>image/jpeg</Format>"));
    assert!(xml.contains("<Format>image/webp</Format>"));
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
    sources: [{ kind: postgis_table, from: t, geometry_column: g }]
    classes:
      - { name: main, title: "Main", style: { type: inline, fill: "#aabbcc" } }
      - { name: minor, title: "Minor", style: { type: inline, stroke: "#555555" } }
"##;
    serde_yaml_ng::from_str(yaml).unwrap()
}

#[test]
fn emits_one_style_per_class_with_rule_param() {
    let cfg = cfg_with_classes();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert_eq!(count_event(&xml, true, |n| n == b"Style"), 2);
    assert!(xml.contains("<Name>main</Name>"));
    assert!(xml.contains("<Name>minor</Name>"));
    assert!(xml.contains("<Title>Main</Title>"));
    assert!(xml.contains("<Title>Minor</Title>"));
    assert!(xml.contains("rule=main"));
    assert!(xml.contains("rule=minor"));
    // class-bound styles never use the synthesised "default" name
    assert!(!xml.contains("<Name>default</Name>"));
}

#[test]
fn classless_layer_keeps_default_style() {
    let cfg = minimal_cfg();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert_eq!(count_event(&xml, true, |n| n == b"Style"), 1);
    assert!(xml.contains("<Name>default</Name>"));
    assert!(xml.contains("<Title>Default style</Title>"));
    assert!(!xml.contains("rule="));
}

#[test]
fn class_title_falls_back_to_name() {
    let mut cfg = cfg_with_classes();
    cfg.layers[0].classes[0].title.clear();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    // class "main" with empty title surfaces the name as title
    assert!(xml.contains("<Name>main</Name>"));
    assert!(xml.contains("<Title>main</Title>"));
}

#[test]
fn empty_class_name_synthesises_stable_identifier() {
    let mut cfg = cfg_with_classes();
    cfg.layers[0].classes[0].name.clear();
    let m = Manifest::empty(1, cfg.service.name.clone());
    let xml = capabilities_xml(&cfg, &m).unwrap();
    assert!(xml.contains("<Name>class-0</Name>"));
    assert!(xml.contains("rule=class-0"));
}
