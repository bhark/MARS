//! WMS GetFeatureInfo response formatters.
//!
//! Three formats: `text/plain`, `text/html`, `application/json`. GML is
//! intentionally absent for now; clients that need it advertise alternative
//! info-formats and most desktop GIS clients accept HTML or JSON.
//!
//! Output shape was chosen to mirror MapServer / GeoServer closely enough
//! that QGIS' identify tool renders results without any layer-specific
//! template work.
//!
//! Per-layer templates ([`GfiTemplates`]) override the default per-feature
//! body when set. The template syntax is the same `{ident}` interpolation
//! used for label `text:`, parsed via [`mars_expr::parse_template`]. When a
//! template is set for a layer:
//! - `text/plain`: the rendered string replaces the `k = v` block for each
//!   feature in that layer.
//! - `text/html`: the rendered string replaces the per-feature attribute
//!   table. The template body is emitted verbatim (no escaping) so operators
//!   can ship HTML markup; identifier values are HTML-escaped before
//!   substitution to prevent attribute-driven injection.
//! - `application/json`: a `"rendered"` string field is added alongside
//!   `"attrs"` carrying the template output. `"attrs"` is preserved so JSON
//!   consumers that ignore the template field keep working.

use std::collections::BTreeMap;

use mars_config::Config;
use mars_expr::{AttributeAccess, Literal, Segment, Template};
use mars_runtime::{AttrValue, LayerFeatureInfo};
use mars_types::LayerId;
use serde_json::{Map, Value, json};

use crate::InfoFormat;

/// MIME -> [`InfoFormat`] mapping. Returns `None` for unsupported formats.
#[must_use]
pub(crate) fn info_format_mime(raw: &str) -> Option<InfoFormat> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("text/plain") {
        Some(InfoFormat::TextPlain)
    } else if trimmed.eq_ignore_ascii_case("text/html") {
        Some(InfoFormat::TextHtml)
    } else if trimmed.eq_ignore_ascii_case("application/json") || trimmed.eq_ignore_ascii_case("application/geo+json") {
        Some(InfoFormat::ApplicationJson)
    } else {
        None
    }
}

/// Parsed per-layer GFI templates, keyed by layer id. Built once at config
/// load and consumed by [`format_feature_info`]. Layers without a template
/// are absent from the map; layers whose template parse failed (validation
/// is a hard pass, but we keep this resilient) are skipped.
#[derive(Debug, Default, Clone)]
pub struct GfiTemplates {
    by_layer: BTreeMap<LayerId, Template>,
}

impl GfiTemplates {
    /// Pre-parse every `Layer.template` in `cfg`. Layers without a template
    /// are skipped. Parse failures are also skipped (the config validator
    /// rejects malformed templates before this is reached).
    #[must_use]
    pub fn from_config(cfg: &Config) -> Self {
        let mut by_layer = BTreeMap::new();
        for layer in &cfg.layers {
            let Some(raw) = layer.template.as_deref() else {
                continue;
            };
            if let Ok(t) = mars_expr::parse_template(raw) {
                by_layer.insert(layer.name.clone(), t);
            }
        }
        Self { by_layer }
    }

    /// True when no layer carries a template (the common case).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_layer.is_empty()
    }

    fn get(&self, layer: &LayerId) -> Option<&Template> {
        self.by_layer.get(layer)
    }
}

/// Format a hit list into the response body for `info_format`. Layers with a
/// matching entry in `templates` get the template-rendered body; others fall
/// back to the default per-format key/value layout.
#[must_use]
pub fn format_feature_info(hits: &[LayerFeatureInfo], info_format: InfoFormat, templates: &GfiTemplates) -> String {
    match info_format {
        InfoFormat::TextPlain => format_text_plain(hits, templates),
        InfoFormat::TextHtml => format_text_html(hits, templates),
        InfoFormat::ApplicationJson => format_json(hits, templates),
    }
}

fn format_text_plain(hits: &[LayerFeatureInfo], templates: &GfiTemplates) -> String {
    // group by layer so all-features-from-one-layer requests aren't visually
    // shuffled. preserves hit order within each layer.
    let mut out = String::new();
    let grouped = group_by_layer(hits);
    for (layer, layer_hits) in grouped {
        let template = templates.get(layer);
        out.push_str("Layer '");
        out.push_str(layer.as_str());
        out.push_str("':\n");
        for h in layer_hits {
            out.push_str("  Feature ");
            out.push_str(&h.user_id.to_string());
            out.push(':');
            out.push('\n');
            if let Some(t) = template {
                // template body replaces the k = v block; indent each line so
                // the template output sits cleanly under the feature header.
                let rendered = render_template(t, h);
                for line in rendered.split('\n') {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            } else {
                for (k, v) in &h.attrs {
                    out.push_str("    ");
                    out.push_str(k);
                    out.push_str(" = ");
                    out.push_str(&attr_value_to_string(v));
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn format_text_html(hits: &[LayerFeatureInfo], templates: &GfiTemplates) -> String {
    let mut out = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>GetFeatureInfo</title></head><body>",
    );
    let grouped = group_by_layer(hits);
    for (layer, layer_hits) in grouped {
        let template = templates.get(layer);
        out.push_str("<h2>");
        out.push_str(&html_escape(layer.as_str()));
        out.push_str("</h2>");
        for h in layer_hits {
            if let Some(t) = template {
                // operator-authored html: emit verbatim. attribute values were
                // html-escaped during template substitution, so injection
                // through `{attr}` is contained.
                out.push_str(&render_template_html(t, h));
            } else {
                out.push_str("<table><caption>Feature ");
                out.push_str(&h.user_id.to_string());
                out.push_str("</caption><thead><tr><th>Attribute</th><th>Value</th></tr></thead><tbody>");
                for (k, v) in &h.attrs {
                    out.push_str("<tr><td>");
                    out.push_str(&html_escape(k));
                    out.push_str("</td><td>");
                    out.push_str(&html_escape(&attr_value_to_string(v)));
                    out.push_str("</td></tr>");
                }
                out.push_str("</tbody></table>");
            }
        }
    }
    out.push_str("</body></html>");
    out
}

fn format_json(hits: &[LayerFeatureInfo], templates: &GfiTemplates) -> String {
    // shape: { "features": [{"layer", "id", "attrs": {k: v, ...}}, ...] }
    // attrs are emitted as a flat object so consumers don't have to peel a
    // tagged-enum envelope around every primitive. when a per-layer template
    // is set, an extra `"rendered"` string field carries the template output;
    // `"attrs"` is preserved so consumers can ignore the rendered string.
    let features: Vec<Value> = hits
        .iter()
        .map(|h| {
            let mut attrs = Map::new();
            for (k, v) in &h.attrs {
                attrs.insert(k.clone(), attr_value_to_json(v));
            }
            let mut feature = json!({
                "layer": h.layer.as_str(),
                "id": h.user_id,
                "attrs": Value::Object(attrs),
            });
            if let Some(t) = templates.get(&h.layer)
                && let Some(obj) = feature.as_object_mut()
            {
                obj.insert("rendered".to_owned(), Value::String(render_template(t, h)));
            }
            feature
        })
        .collect();
    let envelope = json!({ "features": features });
    // serde_json on Vec<u8> is infallible for plain values; if it did fail
    // somehow, fall back to an empty envelope so the response is still
    // well-formed JSON.
    serde_json::to_string(&envelope).unwrap_or_else(|_| r#"{"features":[]}"#.to_owned())
}

fn group_by_layer(hits: &[LayerFeatureInfo]) -> BTreeMap<&mars_types::LayerId, Vec<&LayerFeatureInfo>> {
    let mut out: BTreeMap<&mars_types::LayerId, Vec<&LayerFeatureInfo>> = BTreeMap::new();
    for h in hits {
        out.entry(&h.layer).or_default().push(h);
    }
    out
}

/// Render `template` against the feature's attribute row. Identifier values
/// are emitted raw (no escaping); used by `text/plain` and the JSON
/// `"rendered"` field.
fn render_template(template: &Template, hit: &LayerFeatureInfo) -> String {
    let row = HitAttrs::new(hit);
    mars_expr::eval_template(template, &row).unwrap_or_default()
}

/// HTML-escaping variant: literal segments are emitted verbatim (operator-
/// authored markup), identifier substitutions are HTML-escaped to contain
/// attribute-driven injection.
fn render_template_html(template: &Template, hit: &LayerFeatureInfo) -> String {
    let row = HitAttrs::new(hit);
    let mut out = String::new();
    for seg in &template.segments {
        match seg {
            Segment::Literal(s) => out.push_str(s),
            Segment::Ident(name) => {
                let value = match row.get(name) {
                    None | Some(Literal::Null) => String::new(),
                    Some(Literal::Bool(b)) => b.to_string(),
                    Some(Literal::Int(n)) => n.to_string(),
                    Some(Literal::Float(v)) => v.to_string(),
                    Some(Literal::String(s)) => s,
                };
                out.push_str(&html_escape(&value));
            }
        }
    }
    out
}

/// Bridge between [`AttrValue`] (runtime attribute storage) and
/// [`mars_expr::Literal`] (template evaluator input). 1:1 mapping by
/// construction.
struct HitAttrs<'a> {
    hit: &'a LayerFeatureInfo,
}

impl<'a> HitAttrs<'a> {
    fn new(hit: &'a LayerFeatureInfo) -> Self {
        Self { hit }
    }
}

impl AttributeAccess for HitAttrs<'_> {
    fn get(&self, name: &str) -> Option<Literal> {
        self.hit
            .attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| attr_value_to_literal(v))
    }
}

fn attr_value_to_literal(v: &AttrValue) -> Literal {
    match v {
        AttrValue::Null => Literal::Null,
        AttrValue::Bool(b) => Literal::Bool(*b),
        AttrValue::Int(i) => Literal::Int(*i),
        AttrValue::Float(f) => Literal::Float(*f),
        AttrValue::String(s) => Literal::String(s.clone()),
    }
}

fn attr_value_to_string(v: &AttrValue) -> String {
    match v {
        AttrValue::Null => "null".to_owned(),
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::Int(i) => i.to_string(),
        AttrValue::Float(f) => f.to_string(),
        AttrValue::String(s) => s.clone(),
    }
}

fn attr_value_to_json(v: &AttrValue) -> Value {
    match v {
        AttrValue::Null => Value::Null,
        AttrValue::Bool(b) => Value::Bool(*b),
        AttrValue::Int(i) => Value::from(*i),
        AttrValue::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        AttrValue::String(s) => Value::String(s.clone()),
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
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
}
