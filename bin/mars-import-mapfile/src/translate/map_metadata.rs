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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(kw: &str, args: &[&str]) -> Token {
        Token {
            line: 1,
            keyword: kw.to_string(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn scalar_keys_map_to_fields() {
        let body = vec![
            t("wms_onlineresource", &["https://wms.example/?"]),
            t("ows_encoding", &["UTF-8"]),
            t("ows_fees", &["none"]),
            t("ows_accessconstraints", &["CC-BY 4.0"]),
            t("wms_sld_enabled", &["true"]),
            t("ows_bbox_extended", &["false"]),
            t("ows_abstract", &["a service"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.online_resource.as_deref(), Some("https://wms.example/?"));
        assert_eq!(svc.encoding.as_deref(), Some("UTF-8"));
        assert_eq!(svc.fees.as_deref(), Some("none"));
        assert_eq!(svc.access_constraints.as_deref(), Some("CC-BY 4.0"));
        assert_eq!(svc.sld_enabled, Some(true));
        assert_eq!(svc.bbox_extended, Some(false));
        assert_eq!(svc.abstract_.as_deref(), Some("a service"));
    }

    #[test]
    fn keywords_split_csv() {
        let body = vec![t("ows_keywordlist", &["roads, buildings, parks"])];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.keywords, vec!["roads", "buildings", "parks"]);
    }

    #[test]
    fn keywords_split_whitespace_when_no_commas() {
        let body = vec![t("ows_keywordlist", &["roads buildings parks"])];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.keywords, vec!["roads", "buildings", "parks"]);
    }

    #[test]
    fn srs_list_splits_whitespace() {
        let body = vec![t("ows_srs", &["EPSG:25832 EPSG:4326 EPSG:3857"])];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.advertised_crs, vec!["EPSG:25832", "EPSG:4326", "EPSG:3857"]);
    }

    #[test]
    fn contact_and_address_map_to_fields() {
        let body = vec![
            t("ows_contactperson", &["Pat Operator"]),
            t("ows_contactposition", &["Lead"]),
            t("ows_contactorganization", &["Acme"]),
            t("ows_contactvoicetelephone", &["+1-555-0100"]),
            t("ows_contactfacsimiletelephone", &["+1-555-0101"]),
            t("ows_contactelectronicmailaddress", &["ops@acme"]),
            t("ows_addresstype", &["postal"]),
            t("ows_address", &["1 Main"]),
            t("ows_city", &["Springfield"]),
            t("ows_stateorprovince", &["IL"]),
            t("ows_postcode", &["62701"]),
            t("ows_country", &["US"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.contact_person.as_deref(), Some("Pat Operator"));
        assert_eq!(svc.contact_position.as_deref(), Some("Lead"));
        assert_eq!(svc.contact_organization.as_deref(), Some("Acme"));
        assert_eq!(svc.contact_phone.as_deref(), Some("+1-555-0100"));
        assert_eq!(svc.contact_fax.as_deref(), Some("+1-555-0101"));
        assert_eq!(svc.contact_email.as_deref(), Some("ops@acme"));
        assert_eq!(svc.address_type.as_deref(), Some("postal"));
        assert_eq!(svc.address_street.as_deref(), Some("1 Main"));
        assert_eq!(svc.address_city.as_deref(), Some("Springfield"));
        assert_eq!(svc.address_state.as_deref(), Some("IL"));
        assert_eq!(svc.address_postcode.as_deref(), Some("62701"));
        assert_eq!(svc.address_country.as_deref(), Some("US"));
    }

    #[test]
    fn authorities_pair_by_index_including_unnumbered() {
        let body = vec![
            t("ows_authorityurl_name", &["primary"]),
            t("ows_authorityurl_href", &["https://example.org/p"]),
            t("ows_authorityurl_name1", &["secondary"]),
            t("ows_authorityurl_href1", &["https://example.org/s"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(
            svc.authorities,
            vec![
                ("primary".into(), "https://example.org/p".into()),
                ("secondary".into(), "https://example.org/s".into()),
            ]
        );
    }

    #[test]
    fn identifiers_pair_by_index() {
        let body = vec![
            t("ows_identifier_authority", &["primary"]),
            t("ows_identifier_value", &["urn:a"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.identifiers, vec![("primary".into(), "urn:a".into())]);
    }

    #[test]
    fn authority_with_only_one_side_is_dropped() {
        let body = vec![t("ows_authorityurl_name", &["primary"])];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert!(svc.authorities.is_empty());
    }

    #[test]
    fn format_lists_parsed() {
        let body = vec![
            t("wms_getmap_formatlist", &["image/png,image/jpeg,image/webp"]),
            t("wms_feature_info_mime_type", &["text/html"]),
            t("wms_getlegendgraphic_formatlist", &["image/png"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.getmap_formats, vec!["image/png", "image/jpeg", "image/webp"]);
        assert_eq!(svc.getfeatureinfo_formats, vec!["text/html"]);
        assert_eq!(svc.getlegend_formats, vec!["image/png"]);
    }

    #[test]
    fn unknown_keys_silently_absorbed() {
        let body = vec![
            t("rndr_unknown", &["x"]),
            t("totally_made_up", &["y"]),
            t("wms_onlineresource", &["https://w.example"]),
        ];
        let mut svc = ServiceMetaSkeleton::default();
        parse_map_metadata(&body, &mut svc);
        assert_eq!(svc.online_resource.as_deref(), Some("https://w.example"));
    }
}
