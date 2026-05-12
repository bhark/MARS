//! WMS GetFeatureInfo response formatters.
//!
//! Three formats: `text/plain`, `text/html`, `application/json`. GML is
//! intentionally absent for now; clients that need it advertise alternative
//! info-formats and most desktop GIS clients accept HTML or JSON.
//!
//! Output shape was chosen to mirror MapServer / GeoServer closely enough
//! that QGIS' identify tool renders results without any layer-specific
//! template work.

use std::collections::BTreeMap;

use mars_runtime::{AttrValue, LayerFeatureInfo};
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

/// Format a hit list into the response body for `info_format`.
#[must_use]
pub fn format_feature_info(hits: &[LayerFeatureInfo], info_format: InfoFormat) -> String {
    match info_format {
        InfoFormat::TextPlain => format_text_plain(hits),
        InfoFormat::TextHtml => format_text_html(hits),
        InfoFormat::ApplicationJson => format_json(hits),
    }
}

fn format_text_plain(hits: &[LayerFeatureInfo]) -> String {
    // group by layer so all-features-from-one-layer requests aren't visually
    // shuffled. preserves hit order within each layer.
    let mut out = String::new();
    let grouped = group_by_layer(hits);
    for (layer, layer_hits) in grouped {
        out.push_str("Layer '");
        out.push_str(layer.as_str());
        out.push_str("':\n");
        for h in layer_hits {
            out.push_str("  Feature ");
            out.push_str(&h.user_id.to_string());
            out.push(':');
            out.push('\n');
            for (k, v) in &h.attrs {
                out.push_str("    ");
                out.push_str(k);
                out.push_str(" = ");
                out.push_str(&attr_value_to_string(v));
                out.push('\n');
            }
        }
    }
    out
}

fn format_text_html(hits: &[LayerFeatureInfo]) -> String {
    let mut out = String::from(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <title>GetFeatureInfo</title></head><body>",
    );
    let grouped = group_by_layer(hits);
    for (layer, layer_hits) in grouped {
        out.push_str("<h2>");
        out.push_str(&html_escape(layer.as_str()));
        out.push_str("</h2>");
        for h in layer_hits {
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
    out.push_str("</body></html>");
    out
}

fn format_json(hits: &[LayerFeatureInfo]) -> String {
    // shape: { "features": [{"layer", "id", "attrs": {k: v, ...}}, ...] }
    // attrs are emitted as a flat object so consumers don't have to peel a
    // tagged-enum envelope around every primitive.
    let features: Vec<Value> = hits
        .iter()
        .map(|h| {
            let mut attrs = Map::new();
            for (k, v) in &h.attrs {
                attrs.insert(k.clone(), attr_value_to_json(v));
            }
            json!({
                "layer": h.layer.as_str(),
                "id": h.user_id,
                "attrs": Value::Object(attrs),
            })
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
        let s = format_feature_info(&hits, InfoFormat::TextPlain);
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
        let s = format_feature_info(&hits, InfoFormat::TextHtml);
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
        let s = format_feature_info(&hits, InfoFormat::ApplicationJson);
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
    }

    #[test]
    fn empty_hits_produces_well_formed_payloads() {
        assert_eq!(format_feature_info(&[], InfoFormat::TextPlain), "");
        let html = format_feature_info(&[], InfoFormat::TextHtml);
        assert!(html.starts_with("<!DOCTYPE html>"));
        let json = format_feature_info(&[], InfoFormat::ApplicationJson);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["features"].as_array().unwrap().len(), 0);
    }
}
