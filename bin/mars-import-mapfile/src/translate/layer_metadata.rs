//! LAYER-scope METADATA parser. Translates the `ows_*` / `wms_*` k/v bag
//! that lives inside a `LAYER { METADATA { ... } }` block into the structured
//! fields the emitter consumes. The bag also drives the layer-side
//! request-gating semantics (`wms_enable_request`) and the abstract-parent
//! layer detection (`STATUS OFF` + denied GetMap).

use std::collections::{BTreeMap, BTreeSet};

use crate::scanner::Token;

/// Harvested per-layer WMS metadata. Default values mean "absent" - the
/// emitter renders only fields with content.
#[derive(Debug, Default)]
pub(crate) struct LayerMetadata {
    pub title_override: Option<String>,
    pub abstract_override: Option<String>,
    pub keywords: Vec<String>,
    pub metadata_urls: Vec<MetadataUrlTriple>,
    pub authorities: Vec<(String, String)>,
    pub identifiers: Vec<(String, String)>,
    pub opaque: Option<bool>,
    pub advertised_crs: Vec<String>,
    pub attribution: Option<AttributionTriple>,
    pub include_items: Option<IncludeItemsParsed>,
    pub request_gating: ParsedGating,
}

#[derive(Debug, Default)]
pub(crate) struct MetadataUrlTriple {
    pub type_: String,
    pub format: String,
    pub href: String,
}

#[derive(Debug, Default)]
pub(crate) struct AttributionTriple {
    pub title: Option<String>,
    pub online_resource: Option<String>,
    pub logo_format: Option<String>,
    pub logo_href: Option<String>,
    pub logo_width: Option<u32>,
    pub logo_height: Option<u32>,
}

#[derive(Debug)]
pub(crate) enum IncludeItemsParsed {
    All,
    None,
    Explicit(Vec<String>),
}

/// Parsed per-op gating decisions. `None` means "no explicit setting" - the
/// emitter omits that field so the config consumer falls through to defaults.
/// WMS and WMTS share the struct but track distinct fields per `ServiceOp`
/// variant (e.g. WMS and WMTS each have their own GetCapabilities /
/// GetFeatureInfo).
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedGating {
    pub get_capabilities: Option<bool>,
    pub get_map: Option<bool>,
    pub get_feature_info: Option<bool>,
    pub get_legend_graphic: Option<bool>,
    pub get_styles: Option<bool>,
    pub describe_layer: Option<bool>,
    pub wmts_get_capabilities: Option<bool>,
    pub wmts_get_tile: Option<bool>,
    pub wmts_get_feature_info: Option<bool>,
}

/// Top-level entry: parse a LAYER-body METADATA block.
pub(crate) fn parse_layer_metadata_block(body: &[Token]) -> LayerMetadata {
    let mut m = LayerMetadata::default();
    let mut auth_names: BTreeMap<usize, String> = BTreeMap::new();
    let mut auth_hrefs: BTreeMap<usize, String> = BTreeMap::new();
    let mut ident_auths: BTreeMap<usize, String> = BTreeMap::new();
    let mut ident_values: BTreeMap<usize, String> = BTreeMap::new();
    let mut md_types: BTreeMap<usize, String> = BTreeMap::new();
    let mut md_formats: BTreeMap<usize, String> = BTreeMap::new();
    let mut md_hrefs: BTreeMap<usize, String> = BTreeMap::new();
    let mut attribution: AttributionTriple = AttributionTriple::default();
    let mut attribution_seen = false;

    for t in body {
        let key = t.keyword.to_ascii_lowercase();
        let value = t.args.first().map(String::as_str).unwrap_or("").trim().to_string();

        if try_indexed(&key, "wms_authorityurl_name", &value, &mut auth_names)
            || try_indexed(&key, "ows_authorityurl_name", &value, &mut auth_names)
            || try_indexed(&key, "wms_authorityurl_href", &value, &mut auth_hrefs)
            || try_indexed(&key, "ows_authorityurl_href", &value, &mut auth_hrefs)
            || try_indexed(&key, "wms_identifier_authority", &value, &mut ident_auths)
            || try_indexed(&key, "ows_identifier_authority", &value, &mut ident_auths)
            || try_indexed(&key, "wms_identifier_value", &value, &mut ident_values)
            || try_indexed(&key, "ows_identifier_value", &value, &mut ident_values)
            || try_indexed(&key, "wms_metadataurl_type", &value, &mut md_types)
            || try_indexed(&key, "wms_metadataurl_format", &value, &mut md_formats)
            || try_indexed(&key, "wms_metadataurl_href", &value, &mut md_hrefs)
        {
            continue;
        }
        match key.as_str() {
            "wms_title" | "ows_title" => m.title_override = Some(value),
            "wms_abstract" | "ows_abstract" => m.abstract_override = Some(value),
            "wms_keywordlist" | "ows_keywordlist" => m.keywords.extend(split_keywords(&value)),
            "wms_opaque" => m.opaque = parse_bool(&value),
            "wms_srs" => m.advertised_crs.extend(split_whitespace(&value)),
            "wms_enable_request" => apply_enable_request_wms(&value, &mut m.request_gating),
            "wmts_enable_request" => apply_enable_request_wmts(&value, &mut m.request_gating),
            "ows_include_items" => m.include_items = Some(parse_include_items(&value)),

            "wms_attribution_title" => {
                attribution.title = Some(value);
                attribution_seen = true;
            }
            "wms_attribution_onlineresource" => {
                attribution.online_resource = Some(value);
                attribution_seen = true;
            }
            "wms_attribution_logourl_format" => {
                attribution.logo_format = Some(value);
                attribution_seen = true;
            }
            "wms_attribution_logourl_href" => {
                attribution.logo_href = Some(value);
                attribution_seen = true;
            }
            "wms_attribution_logourl_width" => {
                attribution.logo_width = value.parse().ok();
                attribution_seen = true;
            }
            "wms_attribution_logourl_height" => {
                attribution.logo_height = value.parse().ok();
                attribution_seen = true;
            }

            _ => {} // unknown keys silently absorbed
        }
    }
    flatten_pairs(&mut auth_names, &mut auth_hrefs, &mut m.authorities);
    flatten_pairs(&mut ident_auths, &mut ident_values, &mut m.identifiers);
    m.metadata_urls = flatten_triples(&mut md_types, &mut md_formats, &mut md_hrefs);
    if attribution_seen {
        m.attribution = Some(attribution);
    }
    m
}

fn try_indexed(key: &str, prefix: &str, value: &str, dest: &mut BTreeMap<usize, String>) -> bool {
    let Some(rest) = key.strip_prefix(prefix) else {
        return false;
    };
    let idx = if rest.is_empty() {
        0
    } else if let Ok(n) = rest.parse::<usize>() {
        n
    } else {
        return false;
    };
    dest.insert(idx, value.to_string());
    true
}

fn flatten_pairs(
    left: &mut BTreeMap<usize, String>,
    right: &mut BTreeMap<usize, String>,
    out: &mut Vec<(String, String)>,
) {
    let indices: BTreeSet<usize> = left.keys().chain(right.keys()).copied().collect();
    for i in indices {
        let l = left.remove(&i).unwrap_or_default();
        let r = right.remove(&i).unwrap_or_default();
        if !l.is_empty() && !r.is_empty() {
            out.push((l, r));
        }
    }
}

fn flatten_triples(
    a: &mut BTreeMap<usize, String>,
    b: &mut BTreeMap<usize, String>,
    c: &mut BTreeMap<usize, String>,
) -> Vec<MetadataUrlTriple> {
    let indices: BTreeSet<usize> = a.keys().chain(b.keys()).chain(c.keys()).copied().collect();
    let mut out = Vec::new();
    for i in indices {
        let type_ = a.remove(&i).unwrap_or_default();
        let format = b.remove(&i).unwrap_or_default();
        let href = c.remove(&i).unwrap_or_default();
        if !type_.is_empty() && !format.is_empty() && !href.is_empty() {
            out.push(MetadataUrlTriple { type_, format, href });
        }
    }
    out
}

fn split_csv(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from)
}

fn split_whitespace(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split_whitespace().map(String::from)
}

fn split_keywords(s: &str) -> Vec<String> {
    if s.contains(',') {
        split_csv(s).collect()
    } else {
        split_whitespace(s).collect()
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_include_items(s: &str) -> IncludeItemsParsed {
    match s.trim().to_ascii_lowercase().as_str() {
        "all" => IncludeItemsParsed::All,
        "none" | "" => IncludeItemsParsed::None,
        _ => IncludeItemsParsed::Explicit(split_csv(s).collect()),
    }
}

/// Parse `wms_enable_request` token-by-token into the WMS-side fields of an
/// existing [`ParsedGating`]. Tokens are space-separated: `*` allows every
/// WMS op (sets every unset op to true), `!*` denies every WMS op, `Op`
/// allows that op, `!Op` denies it. Later tokens override earlier ones, so
/// `* !GetStyles` denies only GetStyles. Unrecognised operation names are
/// silently dropped (e.g., `wms_*` prefixes some setups use).
///
/// Mutating an existing struct (rather than returning a fresh one) lets
/// `wmts_enable_request` compose into the same gating when both directives
/// appear on the same layer.
fn apply_enable_request_wms(s: &str, g: &mut ParsedGating) {
    for tok in s.split_whitespace() {
        let (positive, name) = if let Some(rest) = tok.strip_prefix('!') {
            (false, rest)
        } else {
            (true, tok)
        };
        if name == "*" {
            g.get_capabilities = Some(positive);
            g.get_map = Some(positive);
            g.get_feature_info = Some(positive);
            g.get_legend_graphic = Some(positive);
            g.get_styles = Some(positive);
            g.describe_layer = Some(positive);
            continue;
        }
        match name.to_ascii_lowercase().as_str() {
            "getcapabilities" => g.get_capabilities = Some(positive),
            "getmap" => g.get_map = Some(positive),
            "getfeatureinfo" => g.get_feature_info = Some(positive),
            "getlegendgraphic" => g.get_legend_graphic = Some(positive),
            "getstyles" => g.get_styles = Some(positive),
            "describelayer" => g.describe_layer = Some(positive),
            _ => {} // unknown op - silently ignored
        }
    }
}

/// Parse `wmts_enable_request` into the WMTS-side fields of an existing
/// [`ParsedGating`]. Recognised ops mirror MapServer's WMTS surface:
/// GetCapabilities, GetTile, GetFeatureInfo. `*` / `!*` apply to those three.
fn apply_enable_request_wmts(s: &str, g: &mut ParsedGating) {
    for tok in s.split_whitespace() {
        let (positive, name) = if let Some(rest) = tok.strip_prefix('!') {
            (false, rest)
        } else {
            (true, tok)
        };
        if name == "*" {
            g.wmts_get_capabilities = Some(positive);
            g.wmts_get_tile = Some(positive);
            g.wmts_get_feature_info = Some(positive);
            continue;
        }
        match name.to_ascii_lowercase().as_str() {
            "getcapabilities" => g.wmts_get_capabilities = Some(positive),
            "gettile" => g.wmts_get_tile = Some(positive),
            "getfeatureinfo" => g.wmts_get_feature_info = Some(positive),
            _ => {} // unknown op - silently ignored
        }
    }
}

#[cfg(test)]
mod tests;
