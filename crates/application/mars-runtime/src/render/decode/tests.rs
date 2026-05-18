#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_artifact::{ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SpatialIndexBuilder, compute_content_hash};
use mars_render_port::DrawOp;
use mars_style::{Colour, FillPaint, Style, Stylesheet};
use mars_types::{
    Bbox, BindingId, BindingMetadata, CrsCode, DecimationLevel, HilbertKey, ImageFormat, LayerId, PageEntry, PageId,
    PageKey,
};

fn solid(r: u8, g: u8, b: u8) -> Style {
    Style {
        fill: Some(FillPaint::Solid(Colour::rgba(r, g, b, 0xff))),
        ..Default::default()
    }
}

// builds page bytes (spatial index + geometry) for a single 10x10 polygon
// and a class sidecar that maps the slot to one style ref name.
fn build_single_feature_page(style_ref: &str) -> (Bytes, Bytes, Bbox) {
    let feat = FeatureGeom {
        user_id: 1,
        bbox: [0.0, 0.0, 10.0, 10.0],
        geom: GeomKind::Polygon(vec![vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ]]),
    };
    let mut spatial = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE).unwrap();
    spatial.add(0, feat.bbox);
    let spatial_bytes = spatial.finish().unwrap();
    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    let page_bbox = Bbox::new(0.0, 0.0, 10.0, 10.0);
    writer
        .add_spatial_index(spatial_bytes)
        .add_geometry_payload(vec![feat])
        .set_bbox(page_bbox)
        .set_feature_count(1);
    let page_bytes = writer.finish().unwrap();

    let mut writer = ArtifactWriter::new(ArtifactKind::Layer);
    writer
        .add_class_assignment(&[(0u32, 0u16)])
        .add_style_refs(&[style_ref.to_string()])
        .set_bbox(page_bbox);
    let class_bytes = writer.finish().unwrap();
    (page_bytes, class_bytes, page_bbox)
}

fn render_plan_for(bbox: Bbox) -> crate::RenderPlan {
    crate::RenderPlan {
        layers: vec![LayerId::new("L")],
        bbox,
        width: 64,
        height: 64,
        crs: CrsCode::new("EPSG:25832"),
        format: ImageFormat::Png,
        scale_pixel_size_m: crate::OGC_STANDARDIZED_PIXEL_SIZE_M,
    }
}

fn binding_meta(bbox: Bbox) -> BindingMetadata {
    BindingMetadata {
        binding_id: BindingId::try_new("b").unwrap(),
        source_table: "public.x".into(),
        native_crs: CrsCode::new("EPSG:25832"),
        feature_count_total: 1,
        combined_bbox: bbox,
        levels: vec![mars_types::LevelMetadata {
            level: DecimationLevel::new(0),
            vertex_tolerance_m: 0.0,
            geometry_min_size_m: 0.0,
            label_min_priority: 0,
            page_count: 1,
            hilbert_range_table: vec![(HilbertKey::new(0), HilbertKey::new(u64::MAX), PageId::new(1))],
        }],
        page_membership_sidecar: None,
        cycles_since_reconcile: 0,
        last_reconcile_at: None,
    }
}

fn page_entry(bbox: Bbox, page_bytes: &Bytes) -> PageEntry {
    PageEntry {
        key: PageKey {
            binding_id: BindingId::try_new("b").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(1),
        },
        content_hash: compute_content_hash(page_bytes),
        spatial_bbox: bbox,
        hilbert_range: (HilbertKey::new(0), HilbertKey::new(u64::MAX)),
        feature_count: 1,
        size_bytes: page_bytes.len() as u64,
    }
}

#[test]
fn multi_pass_entry_emits_one_drawop_per_pass_in_declared_order() {
    let (page_bytes, class_bytes, bbox) = build_single_feature_page("stack");
    let mut ss = Stylesheet::default();
    let red = solid(0xff, 0, 0);
    let green = solid(0, 0xff, 0);
    let blue = solid(0, 0, 0xff);
    ss.geometry.insert(
        "stack".into(),
        Arc::from(vec![red.clone(), green.clone(), blue.clone()]),
    );

    let plan = render_plan_for(bbox);
    let meta = binding_meta(bbox);
    let pe = page_entry(bbox, &page_bytes);
    let decoded = decode_page_to_ops(
        page_bytes,
        Some(class_bytes),
        &pe,
        &plan,
        &meta,
        &LayerId::new("L"),
        &ss,
        true,
        &[true],
        0,
    )
    .unwrap();

    assert_eq!(decoded.ops.len(), 3, "expected one drawop per declared pass");
    // declared order: red, green, blue. each DrawOp::Path carries the
    // per-pass style; check the fill colour to confirm ordering.
    let pass_fills: Vec<Colour> = decoded
        .ops
        .iter()
        .map(|op| match op {
            DrawOp::Path { style, .. } => match style.fill.as_ref().expect("fill set") {
                FillPaint::Solid(c) => *c,
                _ => panic!("expected solid fill"),
            },
            _ => panic!("expected path op"),
        })
        .collect();
    assert_eq!(pass_fills[0], Colour::rgba(0xff, 0, 0, 0xff));
    assert_eq!(pass_fills[1], Colour::rgba(0, 0xff, 0, 0xff));
    assert_eq!(pass_fills[2], Colour::rgba(0, 0, 0xff, 0xff));
}

#[test]
fn single_pass_entry_emits_one_drawop() {
    let (page_bytes, class_bytes, bbox) = build_single_feature_page("solo");
    let mut ss = Stylesheet::default();
    ss.geometry
        .insert("solo".into(), Arc::from(vec![solid(0x10, 0x20, 0x30)]));

    let plan = render_plan_for(bbox);
    let meta = binding_meta(bbox);
    let pe = page_entry(bbox, &page_bytes);
    let decoded = decode_page_to_ops(
        page_bytes,
        Some(class_bytes),
        &pe,
        &plan,
        &meta,
        &LayerId::new("L"),
        &ss,
        true,
        &[true],
        0,
    )
    .unwrap();
    assert_eq!(decoded.ops.len(), 1);
}

#[test]
fn min_feature_size_gate_drops_pass_below_threshold() {
    // 10x10 metre feature rendered into a 64-px canvas covering 1000x1000
    // metres -> pixel extent 0.64, below an 8 px threshold.
    let (page_bytes, class_bytes, _bbox) = build_single_feature_page("gated");
    let view = Bbox::new(0.0, 0.0, 1000.0, 1000.0);
    let mut ss = Stylesheet::default();
    let mut gated = solid(0, 0, 0);
    gated.min_feature_size_px = Some(8.0);
    let always = solid(0xff, 0xff, 0xff);
    ss.geometry.insert("gated".into(), Arc::from(vec![always, gated]));

    let plan = render_plan_for(view);
    let meta = binding_meta(view);
    let pe = page_entry(view, &page_bytes);
    let decoded = decode_page_to_ops(
        page_bytes,
        Some(class_bytes),
        &pe,
        &plan,
        &meta,
        &LayerId::new("L"),
        &ss,
        true,
        &[true],
        0,
    )
    .unwrap();
    // gated pass dropped; unconditional pass survives.
    assert_eq!(decoded.ops.len(), 1);
}

#[test]
fn min_feature_size_gate_keeps_pass_above_threshold() {
    // 10x10 metre feature rendered into a 64-px canvas covering 20x20
    // metres -> pixel extent 32, above a 4 px threshold.
    let (page_bytes, class_bytes, _bbox) = build_single_feature_page("gated");
    let view = Bbox::new(0.0, 0.0, 20.0, 20.0);
    let mut ss = Stylesheet::default();
    let mut gated = solid(0, 0, 0);
    gated.min_feature_size_px = Some(4.0);
    ss.geometry.insert("gated".into(), Arc::from(vec![gated]));

    let plan = render_plan_for(view);
    let meta = binding_meta(view);
    let pe = page_entry(view, &page_bytes);
    let decoded = decode_page_to_ops(
        page_bytes,
        Some(class_bytes),
        &pe,
        &plan,
        &meta,
        &LayerId::new("L"),
        &ss,
        true,
        &[true],
        0,
    )
    .unwrap();
    assert_eq!(decoded.ops.len(), 1);
}
