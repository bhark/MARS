#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn t(kw: &str, args: &[&str]) -> Token {
    Token {
        line: 1,
        keyword: kw.to_string(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn basic_scalar_keys() {
    let body = vec![
        t("wms_title", &["L Title"]),
        t("wms_abstract", &["L abstract"]),
        t("wms_opaque", &["1"]),
        t("wms_srs", &["EPSG:25832 EPSG:4326"]),
        t("wms_keywordlist", &["a, b, c"]),
    ];
    let m = parse_layer_metadata_block(&body);
    assert_eq!(m.title_override.as_deref(), Some("L Title"));
    assert_eq!(m.abstract_override.as_deref(), Some("L abstract"));
    assert_eq!(m.opaque, Some(true));
    assert_eq!(m.advertised_crs, vec!["EPSG:25832", "EPSG:4326"]);
    assert_eq!(m.keywords, vec!["a", "b", "c"]);
}

fn wms_gating(s: &str) -> ParsedGating {
    let mut g = ParsedGating::default();
    apply_enable_request_wms(s, &mut g);
    g
}

fn wmts_gating(s: &str) -> ParsedGating {
    let mut g = ParsedGating::default();
    apply_enable_request_wmts(s, &mut g);
    g
}

#[test]
fn enable_request_star_then_deny_two() {
    let g = wms_gating("* !GetStyles !DescribeLayer");
    assert_eq!(g.get_capabilities, Some(true));
    assert_eq!(g.get_map, Some(true));
    assert_eq!(g.get_feature_info, Some(true));
    assert_eq!(g.get_legend_graphic, Some(true));
    assert_eq!(g.get_styles, Some(false));
    assert_eq!(g.describe_layer, Some(false));
}

#[test]
fn enable_request_getcaps_only() {
    let g = wms_gating("GetCapabilities !*");
    // `!*` after `GetCapabilities` overrides everything to false including capabilities
    assert_eq!(g.get_capabilities, Some(false));
    assert_eq!(g.get_map, Some(false));
    // tokens applied left-to-right; later wins.
    // Re-do with reversed order:
    let g2 = wms_gating("!* GetCapabilities");
    assert_eq!(g2.get_capabilities, Some(true));
    assert_eq!(g2.get_map, Some(false));
    assert_eq!(g2.get_feature_info, Some(false));
}

#[test]
fn enable_request_bare_deny() {
    let g = wms_gating("!GetCapabilities !GetFeatureInfo");
    assert_eq!(g.get_capabilities, Some(false));
    assert_eq!(g.get_feature_info, Some(false));
    // others unset
    assert_eq!(g.get_map, None);
    assert_eq!(g.get_legend_graphic, None);
}

#[test]
fn wmts_enable_request_star_then_deny_one() {
    let g = wmts_gating("* !GetFeatureInfo");
    assert_eq!(g.wmts_get_capabilities, Some(true));
    assert_eq!(g.wmts_get_tile, Some(true));
    assert_eq!(g.wmts_get_feature_info, Some(false));
    // wms-side untouched by wmts directive
    assert_eq!(g.get_map, None);
    assert_eq!(g.get_capabilities, None);
}

#[test]
fn wmts_enable_request_bare_op() {
    let g = wmts_gating("GetTile");
    assert_eq!(g.wmts_get_tile, Some(true));
    assert_eq!(g.wmts_get_capabilities, None);
}

#[test]
fn wmts_unknown_op_dropped_silently() {
    let g = wmts_gating("GetMap GetLegendGraphic GetTile");
    // GetMap / GetLegendGraphic are WMS-only and must not leak into WMTS fields
    assert_eq!(g.wmts_get_tile, Some(true));
    assert_eq!(g.wmts_get_capabilities, None);
    assert_eq!(g.wmts_get_feature_info, None);
    assert_eq!(g.get_map, None);
}

#[test]
fn wms_and_wmts_directives_compose() {
    // Both directives appearing on the same layer should populate their
    // own sides without clobbering each other.
    let body = vec![
        t("wms_enable_request", &["GetMap GetCapabilities"]),
        t("wmts_enable_request", &["GetTile GetCapabilities"]),
    ];
    let m = parse_layer_metadata_block(&body);
    assert_eq!(m.request_gating.get_map, Some(true));
    assert_eq!(m.request_gating.get_capabilities, Some(true));
    assert_eq!(m.request_gating.wmts_get_tile, Some(true));
    assert_eq!(m.request_gating.wmts_get_capabilities, Some(true));
}

#[test]
fn include_items_modes() {
    let body = vec![t("ows_include_items", &["all"])];
    let m = parse_layer_metadata_block(&body);
    assert!(matches!(m.include_items, Some(IncludeItemsParsed::All)));

    let body = vec![t("ows_include_items", &["none"])];
    let m = parse_layer_metadata_block(&body);
    assert!(matches!(m.include_items, Some(IncludeItemsParsed::None)));

    let body = vec![t("ows_include_items", &["name,class,kind"])];
    let m = parse_layer_metadata_block(&body);
    match m.include_items {
        Some(IncludeItemsParsed::Explicit(names)) => assert_eq!(names, vec!["name", "class", "kind"]),
        other => panic!("expected Explicit, got {other:?}"),
    }
}

#[test]
fn metadata_urls_paired_by_index() {
    let body = vec![
        t("wms_metadataurl_type", &["ISO19115"]),
        t("wms_metadataurl_format", &["text/xml"]),
        t("wms_metadataurl_href", &["https://example.org/md.xml"]),
        t("wms_metadataurl_type1", &["FGDC"]),
        t("wms_metadataurl_format1", &["application/xml"]),
        t("wms_metadataurl_href1", &["https://example.org/fgdc.xml"]),
    ];
    let m = parse_layer_metadata_block(&body);
    assert_eq!(m.metadata_urls.len(), 2);
    assert_eq!(m.metadata_urls[0].type_, "ISO19115");
    assert_eq!(m.metadata_urls[1].type_, "FGDC");
}

#[test]
fn attribution_block_assembled() {
    let body = vec![
        t("wms_attribution_title", &["Acme Maps"]),
        t("wms_attribution_onlineresource", &["https://acme.example"]),
        t("wms_attribution_logourl_format", &["image/png"]),
        t("wms_attribution_logourl_href", &["https://acme.example/logo.png"]),
        t("wms_attribution_logourl_width", &["120"]),
        t("wms_attribution_logourl_height", &["80"]),
    ];
    let m = parse_layer_metadata_block(&body);
    let a = m.attribution.expect("attribution assembled");
    assert_eq!(a.title.as_deref(), Some("Acme Maps"));
    assert_eq!(a.online_resource.as_deref(), Some("https://acme.example"));
    assert_eq!(a.logo_format.as_deref(), Some("image/png"));
    assert_eq!(a.logo_width, Some(120));
    assert_eq!(a.logo_height, Some(80));
}
