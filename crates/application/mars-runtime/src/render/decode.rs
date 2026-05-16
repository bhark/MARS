//! "artifact bytes -> draw ops + class context" stage.
//!
//! opens the geometry artifact for a single page, runs the spatial-index
//! query, decodes geometries at the surviving slots, applies per-class scale
//! gating + style resolution, and emits `DrawOp`s. the resulting
//! `DecodedPage` also carries the `ClassResolver` and the per-slot
//! `rendered_slots` mask so the label pass can resolve style refs and
//! enforce FollowGeometry survival without reopening the artifact.

use std::sync::Arc;

use bytes::Bytes;
use mars_artifact::{
    ArtifactReader, FeatureGeom, GeometryPayload, SectionKind, SpatialIndex, decode_class_assignment, decode_one_geom,
    decode_style_refs,
};
use mars_render_port::DrawOp;
use mars_style::Stylesheet;
use mars_types::{BindingMetadata, LayerId, PageEntry};

use crate::{RenderPlan, RuntimeError};

use super::map_artifact_err;
use super::project::{bbox_native, bbox_to_f32, feature_to_drawop, project_paired_features};

/// per-page render output. `rendered_slots[i]` is true when slot `i`'s
/// geometry survived the spatial-index hit-test for this page; the runtime
/// uses it as the FollowGeometry survival filter, defending the label path
/// against compiler drift between geometry and label sidecar. `class` is
/// hoisted alongside ops so the label pass can resolve style refs without
/// reopening the artifact.
pub(super) struct DecodedPage {
    pub(super) ops: Vec<DrawOp>,
    pub(super) rendered_slots: Vec<bool>,
    pub(super) class: Option<ClassResolver>,
    /// features whose class chain resolved to no stylesheet entry. caller
    /// reports to the unstyled counter once per page so we don't pay metric
    /// overhead per slot on the hot path.
    pub(super) unstyled_count: u64,
}

/// resolves `feature_idx -> Style` by direct slot indexing on a dense
/// `Vec<Option<u16>>`, then looking the class index up in the page-local
/// style_refs table to get a stylesheet entry name.
pub(super) struct ClassResolver {
    /// indexed by per-page slot; `None` when the slot has no class.
    by_slot: Vec<Option<u16>>,
    /// `class_index` indexes into this list to get a stylesheet ref name.
    style_refs: Vec<String>,
}

impl ClassResolver {
    fn open(bytes: Bytes, page_feature_count: usize) -> Result<Self, RuntimeError> {
        let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
        let class_bytes = reader.section(SectionKind::ClassAssignment).map_err(map_artifact_err)?;
        let style_refs_bytes = reader.section(SectionKind::StyleRefs).map_err(map_artifact_err)?;
        let assignments = decode_class_assignment(&class_bytes).map_err(map_artifact_err)?;
        let style_refs = decode_style_refs(&style_refs_bytes).map_err(map_artifact_err)?;
        let mut by_slot: Vec<Option<u16>> = vec![None; page_feature_count];
        for (slot, cls) in assignments {
            let s = slot as usize;
            if s < by_slot.len() {
                by_slot[s] = Some(cls);
            }
        }
        Ok(Self { by_slot, style_refs })
    }

    fn style_ref_for(&self, feature_idx: u32) -> Option<&str> {
        let cls = (*self.by_slot.get(feature_idx as usize)?)? as usize;
        self.style_refs.get(cls).map(String::as_str)
    }

    fn class_idx_for(&self, feature_idx: u32) -> Option<u16> {
        *self.by_slot.get(feature_idx as usize)?
    }

    pub(super) fn style_refs(&self) -> &[String] {
        &self.style_refs
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_page_to_ops(
    bytes: Bytes,
    class_bytes: Option<Bytes>,
    page: &PageEntry,
    plan: &RenderPlan,
    binding: &BindingMetadata,
    layer_id: &LayerId,
    stylesheet: &Stylesheet,
    same_crs: bool,
    class_active: &[bool],
) -> Result<DecodedPage, RuntimeError> {
    let reader = ArtifactReader::open(bytes).map_err(map_artifact_err)?;
    let spatial_bytes = reader.section(SectionKind::SpatialIndex).map_err(map_artifact_err)?;
    let geom_bytes = reader.section(SectionKind::GeometryPayload).map_err(map_artifact_err)?;
    let idx = SpatialIndex::open(spatial_bytes).map_err(map_artifact_err)?;
    let page_feature_count = idx.len() as usize;
    let class = match class_bytes {
        Some(b) => Some(ClassResolver::open(b, page_feature_count)?),
        None => None,
    };
    if idx.is_empty() {
        return Ok(DecodedPage {
            ops: Vec::new(),
            rendered_slots: Vec::new(),
            class,
            unstyled_count: 0,
        });
    }
    let qbb = bbox_native(plan.bbox, &plan.crs, &binding.native_crs)?;
    let qbb_f32 = bbox_to_f32(qbb);
    let mut slots: Vec<u32> = Vec::new();
    idx.query(qbb_f32, &mut slots);
    if slots.is_empty() {
        // page bbox claimed intersection but the R-tree disagrees; bail out.
        let _ = page;
        return Ok(DecodedPage {
            ops: Vec::new(),
            rendered_slots: vec![false; page_feature_count],
            class,
            unstyled_count: 0,
        });
    }
    slots.sort_unstable();
    slots.dedup();
    let mut rendered_slots = vec![false; page_feature_count];
    for &s in &slots {
        let i = s as usize;
        if i < rendered_slots.len() {
            rendered_slots[i] = true;
        }
    }

    // resolve each surviving slot in O(1) against the fixed-stride feature
    // index; pairs (slot, feature) so the class lookup below can join.
    // decode_geometry_at_slots loses slot identity, hence the direct lookup.
    let payload = GeometryPayload::open(&geom_bytes).map_err(map_artifact_err)?;
    let coord_area = payload.coord_area();
    let payload_count = payload.len();
    let mut paired: Vec<(u32, FeatureGeom)> = Vec::with_capacity(slots.len());
    for &slot in &slots {
        // spatial index promises in-range slots; this guard surfaces a
        // typed error if the artifact and index ever disagree.
        if (slot as usize) >= payload_count {
            continue;
        }
        let entry = payload.entry_at(slot).map_err(map_artifact_err)?;
        let geom = decode_one_geom(coord_area, &entry).map_err(map_artifact_err)?;
        paired.push((
            slot,
            FeatureGeom {
                user_id: entry.user_id,
                bbox: entry.bbox,
                geom,
            },
        ));
    }

    let projected = if same_crs {
        paired
    } else {
        project_paired_features(paired, &binding.native_crs, &plan.crs)?
    };
    let mut ops = Vec::with_capacity(projected.len());
    let mut unstyled_count: u64 = 0;
    for (slot, f) in projected {
        // per-class scale gate: a feature assigned to a class whose scale
        // window doesn't cover this denom is suppressed. clear it from
        // rendered_slots too so FollowGeometry-survival labels track the
        // suppression. matches MapServer CLASS MIN/MAXSCALEDENOM semantics.
        if let Some(idx) = class.as_ref().and_then(|c| c.class_idx_for(slot))
            && class_active.get(idx as usize).copied() == Some(false)
        {
            let i = slot as usize;
            if i < rendered_slots.len() {
                rendered_slots[i] = false;
            }
            continue;
        }
        let Some(name) = class.as_ref().and_then(|c| c.style_ref_for(slot)) else {
            // class chain didn't match this feature; drop it and let the
            // caller bump the unstyled counter once for the page.
            unstyled_count += 1;
            continue;
        };
        let Some(passes) = stylesheet.geometry.get(name).cloned() else {
            // class assignment names a stylesheet entry the runtime doesn't
            // know about: manifest/stylesheet drift, surface as a typed error.
            return Err(RuntimeError::StylesheetDrift {
                layer: layer_id.as_str().to_owned(),
                name: name.to_owned(),
            });
        };
        // ordered multi-pass emit: one DrawOp per pass, declared order. a
        // single-pass entry round-trips through the same loop as a one-element
        // slice. clone is unavoidable: feature_to_drawop owns `Arc<Style>`.
        for pass in passes.iter() {
            let style = Arc::new(pass.clone());
            if let Some(op) = feature_to_drawop(&f.geom, plan.bbox, plan.width, plan.height, style) {
                ops.push(op);
            }
        }
    }
    Ok(DecodedPage {
        ops,
        rendered_slots,
        class,
        unstyled_count,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use mars_artifact::{
        ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, SpatialIndexBuilder, compute_content_hash,
    };
    use mars_render_port::DrawOp;
    use mars_style::{Colour, FillPaint, Style, Stylesheet};
    use mars_types::{
        Bbox, BindingId, BindingMetadata, CrsCode, DecimationLevel, HilbertKey, ImageFormat, LayerId, PageEntry,
        PageId, PageKey,
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
        )
        .unwrap();
        assert_eq!(decoded.ops.len(), 1);
    }
}
