#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;
use mars_types::LayerId;

fn hit(layer: &str, user_id: u64, attrs: Vec<(&str, AttrValue)>) -> LayerFeatureInfo {
    LayerFeatureInfo {
        layer: LayerId::new(layer),
        user_id,
        attrs: attrs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
    }
}

fn templates(pairs: &[(&str, &str)]) -> GfiTemplates {
    let mut by_layer = BTreeMap::new();
    for (layer, raw) in pairs {
        by_layer.insert(LayerId::new(*layer), mars_expr::parse_template(raw).unwrap());
    }
    GfiTemplates { by_layer }
}

#[test]
fn mime_lookup() {
    assert_eq!(info_format_mime("text/plain"), Some(InfoFormat::TextPlain));
    assert_eq!(info_format_mime("Text/HTML"), Some(InfoFormat::TextHtml));
    assert_eq!(info_format_mime("application/json"), Some(InfoFormat::ApplicationJson));
    assert_eq!(
        info_format_mime("application/geo+json"),
        Some(InfoFormat::ApplicationJson)
    );
    assert_eq!(info_format_mime("application/vnd.ogc.gml"), None);
}

#[test]
fn plain_groups_by_layer_and_lists_attrs() {
    let hits = vec![
        hit(
            "roads",
            42,
            vec![("name", AttrValue::String("A1".into())), ("lanes", AttrValue::Int(4))],
        ),
        hit("parks", 7, vec![("area_m2", AttrValue::Float(1234.5))]),
    ];
    let s = format_feature_info(&hits, InfoFormat::TextPlain, &GfiTemplates::default());
    assert!(s.contains("Layer 'roads':"));
    assert!(s.contains("Feature 42:"));
    assert!(s.contains("name = A1"));
    assert!(s.contains("lanes = 4"));
    assert!(s.contains("Layer 'parks':"));
    assert!(s.contains("area_m2 = 1234.5"));
}

#[test]
fn html_escapes_payload() {
    let hits = vec![hit("roads", 1, vec![("notes", AttrValue::String("a & b <c>".into()))])];
    let s = format_feature_info(&hits, InfoFormat::TextHtml, &GfiTemplates::default());
    assert!(s.contains("a &amp; b &lt;c&gt;"));
    assert!(!s.contains("a & b <c>"));
}

#[test]
fn json_emits_flat_attrs_map() {
    let hits = vec![hit(
        "roads",
        42,
        vec![
            ("name", AttrValue::String("A1".into())),
            ("lanes", AttrValue::Int(4)),
            ("active", AttrValue::Bool(true)),
            ("missing", AttrValue::Null),
        ],
    )];
    let s = format_feature_info(&hits, InfoFormat::ApplicationJson, &GfiTemplates::default());
    let v: Value = serde_json::from_str(&s).unwrap();
    let features = v["features"].as_array().unwrap();
    assert_eq!(features.len(), 1);
    let f = &features[0];
    assert_eq!(f["layer"], "roads");
    assert_eq!(f["id"], 42);
    assert_eq!(f["attrs"]["name"], "A1");
    assert_eq!(f["attrs"]["lanes"], 4);
    assert_eq!(f["attrs"]["active"], true);
    assert_eq!(f["attrs"]["missing"], Value::Null);
    // no template configured for this layer -> no `rendered` field.
    assert!(f.get("rendered").is_none());
}

#[test]
fn empty_hits_produces_well_formed_payloads() {
    let t = GfiTemplates::default();
    assert_eq!(format_feature_info(&[], InfoFormat::TextPlain, &t), "");
    let html = format_feature_info(&[], InfoFormat::TextHtml, &t);
    assert!(html.starts_with("<!DOCTYPE html>"));
    let json = format_feature_info(&[], InfoFormat::ApplicationJson, &t);
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["features"].as_array().unwrap().len(), 0);
}

#[test]
fn plain_template_overrides_kv_block_per_layer() {
    let hits = vec![
        hit(
            "roads",
            1,
            vec![("name", AttrValue::String("A1".into())), ("lanes", AttrValue::Int(4))],
        ),
        hit("parks", 7, vec![("area_m2", AttrValue::Float(1234.5))]),
    ];
    let t = templates(&[("roads", "name={name}, lanes={lanes}")]);
    let s = format_feature_info(&hits, InfoFormat::TextPlain, &t);
    // roads renders via template
    assert!(s.contains("    name=A1, lanes=4"));
    assert!(!s.contains("name = A1"));
    // parks (untemplated) falls back to the default k = v block
    assert!(s.contains("    area_m2 = 1234.5"));
}

#[test]
fn html_template_emits_markup_verbatim_and_escapes_idents() {
    let hits = vec![hit("roads", 1, vec![("name", AttrValue::String("A & B <C>".into()))])];
    let t = templates(&[("roads", "<p class=\"feat\">{name}</p>")]);
    let s = format_feature_info(&hits, InfoFormat::TextHtml, &t);
    // operator markup survives unchanged
    assert!(s.contains("<p class=\"feat\">"));
    assert!(s.contains("</p>"));
    // identifier value is escaped to prevent injection
    assert!(s.contains("A &amp; B &lt;C&gt;"));
    assert!(!s.contains("A & B <C>"));
    // default per-feature table is not emitted
    assert!(!s.contains("<th>Attribute</th>"));
}

#[test]
fn json_adds_rendered_field_when_template_set() {
    let hits = vec![hit(
        "roads",
        42,
        vec![("name", AttrValue::String("A1".into())), ("lanes", AttrValue::Int(4))],
    )];
    let t = templates(&[("roads", "{name}/{lanes}")]);
    let s = format_feature_info(&hits, InfoFormat::ApplicationJson, &t);
    let v: Value = serde_json::from_str(&s).unwrap();
    let f = &v["features"][0];
    assert_eq!(f["rendered"], "A1/4");
    // attrs is preserved
    assert_eq!(f["attrs"]["name"], "A1");
    assert_eq!(f["attrs"]["lanes"], 4);
}

#[test]
fn template_renders_missing_attr_as_empty_string() {
    let hits = vec![hit("roads", 1, vec![("name", AttrValue::String("A1".into()))])];
    let t = templates(&[("roads", "{name}-{missing}")]);
    let s = format_feature_info(&hits, InfoFormat::TextPlain, &t);
    assert!(s.contains("    A1-\n"));
}

#[test]
fn from_config_skips_layers_without_template_and_parses_others() {
    // round-trip via the canonical minimal yaml shape; two layers, one
    // with `template:` set, exercises the from_config walk end-to-end.
    let yaml = r#"
service:
  name: demo
  title: demo
sources:
  - id: default
    type: postgis
    dsn: "postgres://localhost/x"
    native_crs: EPSG:25832
artifacts:
  store: { type: fs, path: /tmp }
  cache: { path: /tmp, max_size: 1GiB, eviction: lru }
scales:
  bands: []
cells:
  grid: regular
  origin: [0, 0]
  size_per_band: {}
interfaces:
  wms: { enabled: true, versions: ["1.3.0"], formats: ["image/png"] }
layers:
  - { name: a, type: polygon, sources: [] }
  - { name: b, type: polygon, sources: [], template: "hello {x}" }
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let t = GfiTemplates::from_config(&cfg);
    assert!(t.get(&LayerId::new("a")).is_none());
    assert!(t.get(&LayerId::new("b")).is_some());
}
