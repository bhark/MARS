#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_artifact::FeatureGeom;
use mars_source::AttrValue;
use mars_types::{BindingId, LayerId};
use std::sync::Arc;

fn keyed_row(user_id: u64, kind: &str, key: u64) -> KeyedRow {
    KeyedRow {
        feature: FeatureGeom {
            user_id,
            bbox: [0.0, 0.0, 1.0, 1.0],
            geom: mars_artifact::GeomKind::Point((0.0, 0.0)),
        },
        attrs: Arc::new(vec![("kind".into(), AttrValue::String(kind.into()))]),
        geom_bytes_estimate: 16,
        key: HilbertKey::new(key),
        row_fingerprint: user_id,
    }
}

fn layer_with_classes(name: &str, when_exprs: &[Option<&str>]) -> crate::plan::LayerPlan {
    let classes = when_exprs
        .iter()
        .enumerate()
        .map(|(i, w)| crate::plan::ClassPlan {
            name: format!("c{i}"),
            when: w.map(|s| mars_expr::parse(s).unwrap()),
            style_ref: format!("{name}__c{i}"),
            label: None,
        })
        .collect();
    crate::plan::LayerPlan {
        layer_id: LayerId::new(name),
        binding_id: BindingId::try_new(name).unwrap(),
        kind: "geom".into(),
        classes,
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
    }
}

#[test]
fn filter_unmatched_rows_drops_rows_that_match_no_layer() {
    let layer = layer_with_classes("roads", &[Some("kind = 'major'")]);
    let layers: Vec<&crate::plan::LayerPlan> = vec![&layer];
    let rows = vec![
        keyed_row(1, "major", 10),
        keyed_row(2, "minor", 20),
        keyed_row(3, "major", 30),
    ];
    let (kept, dropped) = filter_unmatched_rows(rows, &layers);
    assert_eq!(dropped, 1);
    let ids: Vec<u64> = kept.iter().map(|r| r.feature.user_id).collect();
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn filter_unmatched_rows_keeps_all_when_a_layer_has_no_classes() {
    // a label-only layer (no classes) means we cannot authoritatively
    // drop anything: keep all rows so its labels still emit.
    let label_only = crate::plan::LayerPlan {
        layer_id: LayerId::new("labels"),
        binding_id: BindingId::try_new("labels").unwrap(),
        kind: "geom".into(),
        classes: Vec::new(),
        label: None,
        label_survival: mars_style::LabelSurvival::Independent,
    };
    let layers: Vec<&crate::plan::LayerPlan> = vec![&label_only];
    let rows = vec![keyed_row(1, "anything", 10), keyed_row(2, "else", 20)];
    let (kept, dropped) = filter_unmatched_rows(rows, &layers);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2);
}

#[test]
fn filter_unmatched_rows_keeps_row_that_matches_any_layer() {
    // shared-binding case: layer A matches "major", layer B matches
    // "minor". a row labelled "minor" must survive because B keeps it.
    let a = layer_with_classes("a", &[Some("kind = 'major'")]);
    let b = layer_with_classes("b", &[Some("kind = 'minor'")]);
    let layers: Vec<&crate::plan::LayerPlan> = vec![&a, &b];
    let rows = vec![
        keyed_row(1, "major", 10),
        keyed_row(2, "minor", 20),
        keyed_row(3, "path", 30),
    ];
    let (kept, dropped) = filter_unmatched_rows(rows, &layers);
    assert_eq!(dropped, 1);
    let ids: Vec<u64> = kept.iter().map(|r| r.feature.user_id).collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn filter_unmatched_rows_keeps_all_under_catch_all_class() {
    // a `None` when-clause is the catch-all; assign_class returns Some
    // for it, so no row should be dropped.
    let layer = layer_with_classes("any", &[None]);
    let layers: Vec<&crate::plan::LayerPlan> = vec![&layer];
    let rows = vec![keyed_row(1, "x", 10), keyed_row(2, "y", 20)];
    let (kept, dropped) = filter_unmatched_rows(rows, &layers);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 2);
}

fn label_plan(style_ref: &str) -> crate::plan::LayerLabelPlan {
    crate::plan::LayerLabelPlan {
        style_ref: style_ref.into(),
        style: mars_style::LabelStyle {
            font_family: "DejaVu Sans".into(),
            font_size: 12.0.into(),
            fill: mars_style::Colour::rgb(0, 0, 0),
            halo: None,
            priority: 0,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle: None,
            partials: false,
            force: false,
        },
        text: mars_expr::parse_template("{name}").unwrap(),
        placement: mars_style::Placement::Point,
    }
}

#[test]
fn style_refs_layout_geom_then_class_labels_then_layer_label() {
    // shape: 3 classes; class 0 and class 2 have per-class labels; layer
    // has a fallback label. expected style_refs order:
    // [geom0, geom1, geom2, classlabel0, classlabel2, layerlabel].
    let mut layer = layer_with_classes("vejnavne", &[Some("kind = 'major'"), None, None]);
    layer.classes[0].label = Some(label_plan("vejnavne__major__label"));
    layer.classes[2].label = Some(label_plan("vejnavne__other__label"));
    layer.label = Some(label_plan("vejnavne__label"));

    let refs = build_layer_style_refs(&layer).unwrap();
    assert_eq!(
        refs.style_refs_full,
        vec![
            "vejnavne__c0".to_string(),
            "vejnavne__c1".to_string(),
            "vejnavne__c2".to_string(),
            "vejnavne__major__label".to_string(),
            "vejnavne__other__label".to_string(),
            "vejnavne__label".to_string(),
        ]
    );
    assert_eq!(refs.class_label_indices, vec![Some(3), None, Some(4)]);
    assert_eq!(refs.layer_label_index, Some(5));
}

#[test]
fn style_refs_layout_no_labels_omits_label_slots() {
    let layer = layer_with_classes("roads", &[Some("kind = 'main'"), None]);
    let refs = build_layer_style_refs(&layer).unwrap();
    assert_eq!(
        refs.style_refs_full,
        vec!["roads__c0".to_string(), "roads__c1".to_string()]
    );
    assert_eq!(refs.class_label_indices, vec![None, None]);
    assert_eq!(refs.layer_label_index, None);
}

#[test]
fn style_refs_layout_only_layer_label_keeps_today_layout() {
    // pre-existing shape: classes have no labels, only the layer does.
    // the layer-label idx must still equal classes.len() (today's
    // invariant) so existing label sidecars decode unchanged.
    let mut layer = layer_with_classes("a", &[Some("k = '1'"), Some("k = '2'")]);
    layer.label = Some(label_plan("a__label"));
    let refs = build_layer_style_refs(&layer).unwrap();
    assert_eq!(refs.style_refs_full.len(), 3);
    assert_eq!(refs.class_label_indices, vec![None, None]);
    assert_eq!(refs.layer_label_index, Some(2));
}
