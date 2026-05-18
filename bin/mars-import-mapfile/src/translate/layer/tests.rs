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
fn composite_opacity_and_compop_lower_together() {
    let body = vec![t("OPACITY", &["50"]), t("COMPOP", &["multiply"])];
    let p = parse_composite(&body);
    assert!((p.opacity.unwrap() - 0.5).abs() < f32::EPSILON);
    assert_eq!(p.blend_mode, Some(mars_style::BlendMode::Multiply));
}

#[test]
fn composite_compop_unknown_falls_through_to_none() {
    // unrecognised COMPOP triggers a warn but does not set blend_mode,
    // so the renderer falls back to source-over.
    let body = vec![t("COMPOP", &["color-burn"])];
    let p = parse_composite(&body);
    assert_eq!(p.blend_mode, None);
}

#[test]
fn composite_compop_normal_maps_to_source_over() {
    let body = vec![t("COMPOP", &["normal"])];
    let p = parse_composite(&body);
    assert_eq!(p.blend_mode, Some(mars_style::BlendMode::SourceOver));
}

#[test]
fn composite_filter_emits_no_blend_mode_and_no_opacity() {
    // FILTER is recognised but not supported; it triggers a warn and is
    // otherwise dropped. The block leaves the other fields untouched.
    let body = vec![t("FILTER", &["(some expr)"])];
    let p = parse_composite(&body);
    assert_eq!(p.opacity, None);
    assert_eq!(p.blend_mode, None);
}

#[test]
fn composite_compfilter_silently_ignored() {
    // COMPFILTER is mapserver expression syntax and explicitly out of
    // scope. No warn, no field set.
    let body = vec![t("COMPFILTER", &["[type] = 'major'"])];
    let p = parse_composite(&body);
    assert_eq!(p.opacity, None);
    assert_eq!(p.blend_mode, None);
}
