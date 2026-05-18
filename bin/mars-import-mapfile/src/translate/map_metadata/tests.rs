#![allow(clippy::unwrap_used)]

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
