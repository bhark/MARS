//! MAP-scope METADATA parser.
//!
//! Translates the `ows_*` and `wms_*` k/v bag at MAP scope into the
//! structured fields the YAML emitter consumes. Unknown keys are silently
//! absorbed - mapfile METADATA is intentionally a free-form bag and a strict
//! mode is a future concern.

use std::collections::{BTreeMap, BTreeSet};

use crate::emitter::ServiceMetaSkeleton;
use crate::scanner::Token;

/// Parse a single MAP-scope METADATA block body into `svc`. Repeated calls
/// merge keys; later writes win for scalar fields, later entries append for
/// list fields.
pub(crate) fn parse_map_metadata(body: &[Token], svc: &mut ServiceMetaSkeleton) {
    let mut auth_names: BTreeMap<usize, String> = BTreeMap::new();
    let mut auth_hrefs: BTreeMap<usize, String> = BTreeMap::new();
    let mut ident_auths: BTreeMap<usize, String> = BTreeMap::new();
    let mut ident_values: BTreeMap<usize, String> = BTreeMap::new();

    for t in body {
        let key = t.keyword.to_ascii_lowercase();
        let value = t.args.first().map(String::as_str).unwrap_or("").trim().to_string();
        if try_indexed(&key, "ows_authorityurl_name", &value, &mut auth_names)
            || try_indexed(&key, "wms_authorityurl_name", &value, &mut auth_names)
            || try_indexed(&key, "ows_authorityurl_href", &value, &mut auth_hrefs)
            || try_indexed(&key, "wms_authorityurl_href", &value, &mut auth_hrefs)
            || try_indexed(&key, "ows_identifier_authority", &value, &mut ident_auths)
            || try_indexed(&key, "wms_identifier_authority", &value, &mut ident_auths)
            || try_indexed(&key, "ows_identifier_value", &value, &mut ident_values)
            || try_indexed(&key, "wms_identifier_value", &value, &mut ident_values)
        {
            continue;
        }
        match key.as_str() {
            "wms_onlineresource" | "ows_onlineresource" => {
                svc.online_resource = Some(value);
            }
            "ows_keywordlist" | "wms_keywordlist" => {
                svc.keywords.extend(split_keywords(&value));
            }
            "ows_encoding" => svc.encoding = Some(value),
            "ows_bbox_extended" => svc.bbox_extended = parse_bool(&value),
            "wms_sld_enabled" => svc.sld_enabled = parse_bool(&value),
            "ows_srs" | "wms_srs" => {
                svc.advertised_crs.extend(split_whitespace(&value));
            }
            "ows_fees" | "wms_fees" => svc.fees = Some(value),
            "ows_accessconstraints" | "wms_accessconstraints" => svc.access_constraints = Some(value),
            "wms_title" | "ows_title" => svc.title_override = Some(value),
            "ows_abstract" | "wms_abstract" => svc.abstract_ = Some(value),

            "ows_contactperson" | "wms_contactperson" => svc.contact_person = Some(value),
            "ows_contactposition" | "wms_contactposition" => svc.contact_position = Some(value),
            "ows_contactorganization" | "wms_contactorganization" => svc.contact_organization = Some(value),
            "ows_contactvoicetelephone" | "wms_contactvoicetelephone" => svc.contact_phone = Some(value),
            "ows_contactfacsimiletelephone" | "wms_contactfacsimiletelephone" => svc.contact_fax = Some(value),
            "ows_contactelectronicmailaddress" | "wms_contactelectronicmailaddress" => {
                svc.contact_email = Some(value);
            }

            "ows_addresstype" | "wms_addresstype" => svc.address_type = Some(value),
            "ows_address" | "wms_address" => svc.address_street = Some(value),
            "ows_city" | "wms_city" => svc.address_city = Some(value),
            "ows_stateorprovince" | "wms_stateorprovince" => svc.address_state = Some(value),
            "ows_postcode" | "wms_postcode" => svc.address_postcode = Some(value),
            "ows_country" | "wms_country" => svc.address_country = Some(value),

            "wms_getmap_formatlist" => svc.getmap_formats.extend(split_csv(&value)),
            "wms_feature_info_mime_type" => svc.getfeatureinfo_formats.push(value),
            "wms_getlegendgraphic_formatlist" => svc.getlegend_formats.extend(split_csv(&value)),

            _ => {} // unknown keys silently absorbed; --strict mode is a future concern
        }
    }

    flatten_pairs(&mut auth_names, &mut auth_hrefs, &mut svc.authorities);
    flatten_pairs(&mut ident_auths, &mut ident_values, &mut svc.identifiers);
}

/// Recognise an indexed metadata key like `<prefix>` (entry 0) or
/// `<prefix><N>` (entry N). On match, stores the value into `dest` keyed by
/// the index and returns true. The empty-suffix and `0`-suffix forms both
/// land at index 0 so we don't double-store.
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

/// Zip two index-keyed maps into a `Vec<(left, right)>`, dropping entries
/// where either side is empty. Stable ordering by index.
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

fn split_csv(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from)
}

fn split_whitespace(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split_whitespace().map(String::from)
}

/// MapServer keyword lists are comma-separated; whitespace-only inputs fall
/// back to whitespace splitting so single-word lists still parse.
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

#[cfg(test)]
mod tests;
