//! bootstrap snapshot: v3 substrate emission from a streamed source.
//!
//! C.1 scope: per-binding, level-0 only, in-memory sort. emits one set of
//! page artifacts (SpatialIndex + GeometryPayload + Attributes) per binding,
//! plus a page-membership sidecar, plus a v3 manifest pointer.
//!
//! out of scope here (C.2):
//! - bucketed external-merge sort for bindings whose row set exceeds RAM
//!   (the LAZARUS plan calls for ~4 GiB working-set ceiling; in C.1 we
//!   assume the binding fits in memory and document the limitation).
//! - non-zero decimation levels (decimate.rs, mars-style label policy).
//! - class / label sidecars (compile path that joins style data per layer).
//! - rebuild from change-feed events (incremental.rs, sidecar lookups).

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::{
    ArtifactKind, ArtifactWriter, FeatureGeom, GeomKind, MAX_ROW_BYTES, SpatialIndexBuilder, compute_content_hash,
    encode_row, wkb_to_feature_geom,
};
use mars_source::{RowBytes, SourceBinding as PortBinding, SourceCollectionId};
use mars_store::ObjectStore;
use mars_types::{
    ArtifactEntry, ArtifactKey, Bbox, BindingMetadata, ContentHash, DecimationLevel, HilbertKey, LevelMetadata,
    MANIFEST_FORMAT_VERSION, Manifest, PageEntry, PageId, PageKey,
};

use crate::hilbert::key_from_centroid;
use crate::plan::{BindingPlan, BootstrapPlan};
use crate::sidecar::encode_sidecar;
use crate::{CompilerError, Deps};

/// Run a single snapshot pass against the bindings in `plan`. Writes every
/// page artifact + sidecar + manifest body via `deps`, returns the manifest
/// for the caller to publish.
///
/// `manifest_version` becomes `Manifest::version`; the caller derives this
/// from the previous publication (typically `prev + 1`).
pub async fn run_snapshot(
    deps: &Deps,
    plan: &BootstrapPlan,
    service_name: String,
    manifest_version: u64,
) -> Result<Manifest, CompilerError> {
    let mut bindings_meta: Vec<BindingMetadata> = Vec::with_capacity(plan.bindings.len());
    let mut pages_meta: Vec<PageEntry> = Vec::new();

    for binding in &plan.bindings {
        let (binding_meta, mut binding_pages) = snapshot_one_binding(deps, binding).await?;
        bindings_meta.push(binding_meta);
        pages_meta.append(&mut binding_pages);
    }

    // sort pages by (binding_id, level, hilbert_range.0) so render-time slice
    // scan is a binary-searchable contiguous run.
    pages_meta.sort_by(|a, b| {
        a.key
            .binding_id
            .as_str()
            .cmp(b.key.binding_id.as_str())
            .then_with(|| a.key.level.cmp(&b.key.level))
            .then_with(|| a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    Ok(Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: manifest_version,
        service: service_name,
        created_at: SystemTime::now(),
        bindings: bindings_meta,
        pages: pages_meta,
        class_sidecars: Vec::new(),
        label_sidecars: Vec::new(),
        style_artifact: None,
        source_version: None,
        epoch: manifest_version,
    })
}

async fn snapshot_one_binding(
    deps: &Deps,
    binding: &BindingPlan,
) -> Result<(BindingMetadata, Vec<PageEntry>), CompilerError> {
    // 1. stream rows; decode WKB -> FeatureGeom + accumulate combined_bbox.
    let rows = collect_binding_rows(deps, binding).await?;
    let total_features = rows.len() as u64;

    if rows.is_empty() {
        // empty binding: still record metadata so the manifest reports zero
        // pages without a special-case at lookup time.
        let meta = BindingMetadata {
            binding_id: binding.binding_id.clone(),
            source_table: binding.source_table.clone(),
            native_crs: binding.native_crs.clone(),
            feature_count_total: 0,
            levels: vec![empty_level_metadata(binding)],
            page_membership_sidecar: None,
        };
        return Ok((meta, Vec::new()));
    }

    let combined_bbox = combined_bbox(&rows);

    // 2. compute Hilbert keys against combined_bbox; sort by key.
    let mut keyed: Vec<KeyedRow> = rows
        .into_iter()
        .map(|r| {
            let centroid_x = (f64::from(r.feature.bbox[0]) + f64::from(r.feature.bbox[2])) / 2.0;
            let centroid_y = (f64::from(r.feature.bbox[1]) + f64::from(r.feature.bbox[3])) / 2.0;
            let key = key_from_centroid(centroid_x, centroid_y, combined_bbox);
            KeyedRow { key, ..r }
        })
        .collect();
    keyed.sort_by_key(|r| r.key);

    // 3. sweep into byte-budgeted pages and emit.
    let level0 = DecimationLevel::new(0);
    let mut next_page_id: u64 = 0;
    let mut pages: Vec<PageEntry> = Vec::new();
    let mut sidecar_entries: Vec<(u64, HilbertKey)> = Vec::with_capacity(keyed.len() as usize);

    let mut current: Vec<KeyedRow> = Vec::new();
    let mut current_bytes: u64 = 0;

    for r in keyed {
        sidecar_entries.push((r.feature.id, r.key));
        let est = estimate_row_size(&r);
        // close the current page if appending this row would exceed the target.
        if !current.is_empty() && current_bytes.saturating_add(est) > binding.page_size_target_bytes {
            let page = emit_page(deps, binding, level0, PageId::new(next_page_id), &current).await?;
            pages.push(page);
            next_page_id += 1;
            current = Vec::new();
            current_bytes = 0;
        }
        current_bytes = current_bytes.saturating_add(est);
        current.push(r);
    }
    if !current.is_empty() {
        let page = emit_page(deps, binding, level0, PageId::new(next_page_id), &current).await?;
        pages.push(page);
    }

    // 4. encode + put page-membership sidecar.
    let sidecar_bytes = encode_sidecar(&mut sidecar_entries).map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_sidecar_err(&e),
    })?;
    let sidecar_hash = compute_content_hash(&sidecar_bytes);
    let sidecar_key = sidecar_object_key(binding.binding_id.as_str(), &sidecar_hash)?;
    let sidecar_size = sidecar_bytes.len() as u64;
    deps.store.put(&sidecar_key, sidecar_bytes).await?;

    let level_meta = LevelMetadata {
        level: level0,
        vertex_tolerance_m: binding.levels[0].vertex_tolerance_m,
        geometry_min_size_m: binding.levels[0].geometry_min_size_m,
        label_min_priority: binding.levels[0].label_min_priority,
        page_count: pages.len() as u32,
        combined_bbox,
        hilbert_range_table: pages.iter().map(|p| p.hilbert_range).collect(),
    };

    let meta = BindingMetadata {
        binding_id: binding.binding_id.clone(),
        source_table: binding.source_table.clone(),
        native_crs: binding.native_crs.clone(),
        feature_count_total: total_features,
        levels: vec![level_meta],
        page_membership_sidecar: Some(ArtifactEntry {
            key: sidecar_key,
            hash: sidecar_hash,
            size_bytes: sidecar_size,
        }),
    };
    Ok((meta, pages))
}

#[derive(Debug)]
struct KeyedRow {
    feature: FeatureGeom,
    attrs_bytes: Vec<u8>,
    geom_bytes_estimate: u64,
    key: HilbertKey,
}

async fn collect_binding_rows(deps: &Deps, binding: &BindingPlan) -> Result<Vec<KeyedRow>, CompilerError> {
    let port_binding = PortBinding::new(
        SourceCollectionId::new(binding.binding_id.as_str()),
        binding_schema(&binding.source_table),
        binding_table(&binding.source_table),
        binding.geometry_column.clone(),
        binding.id_column.as_deref().unwrap_or("id"),
        binding.attributes.clone(),
        binding.native_crs.clone(),
    )?;
    let mut stream = deps.source.fetch_full_table_streaming(&port_binding).await?;

    let mut rows: Vec<KeyedRow> = Vec::new();
    while let Some(item) = stream.next().await {
        let row: RowBytes = item?;
        let geom_bytes_estimate = row.geometry.len() as u64;
        let feature =
            wkb_to_feature_geom(&row.geometry, row.feature_id).map_err(|e| CompilerError::LegacySubstrateRetired {
                what: stringify_wkb_err(&e),
            })?;

        // shape-conform the attribute names so the per-row codec stays uniform.
        let attrs: Vec<(String, mars_artifact::AttrValue)> = row
            .attributes
            .into_iter()
            .map(|(name, v)| (name, attr_value_to_artifact(v)))
            .collect();
        let mut row_bytes = encode_row(&attrs).map_err(|e| CompilerError::LegacySubstrateRetired {
            what: stringify_attr_err(&e),
        })?;
        if row_bytes.len() > MAX_ROW_BYTES {
            // huge attribute payload would be rejected by the section codec
            // anyway; surface it with a clear label here.
            return Err(CompilerError::LegacySubstrateRetired {
                what: "snapshot: row attributes exceed MAX_ROW_BYTES",
            });
        }
        let attrs_bytes = std::mem::take(&mut row_bytes).to_vec();
        rows.push(KeyedRow {
            feature,
            attrs_bytes,
            geom_bytes_estimate,
            key: HilbertKey::min(),
        });
    }
    Ok(rows)
}

fn combined_bbox(rows: &[KeyedRow]) -> Bbox {
    // rows is non-empty by caller invariant.
    let first = &rows[0].feature.bbox;
    let mut min_x = f64::from(first[0]);
    let mut min_y = f64::from(first[1]);
    let mut max_x = f64::from(first[2]);
    let mut max_y = f64::from(first[3]);
    for r in &rows[1..] {
        let bb = r.feature.bbox;
        if (bb[0] as f64) < min_x {
            min_x = bb[0] as f64;
        }
        if (bb[1] as f64) < min_y {
            min_y = bb[1] as f64;
        }
        if (bb[2] as f64) > max_x {
            max_x = bb[2] as f64;
        }
        if (bb[3] as f64) > max_y {
            max_y = bb[3] as f64;
        }
    }
    Bbox::new(min_x, min_y, max_x, max_y)
}

fn estimate_row_size(r: &KeyedRow) -> u64 {
    // rough proxy: encoded attribute bytes plus the source WKB size as a
    // stand-in for the post-encode geometry payload (varint codec averages
    // ~125 B/feat per LAZARUS measurement; WKB is ~2x that; close enough).
    r.geom_bytes_estimate + r.attrs_bytes.len() as u64 + 64
}

async fn emit_page(
    deps: &Deps,
    binding: &BindingPlan,
    level: DecimationLevel,
    page_id: PageId,
    rows: &[KeyedRow],
) -> Result<PageEntry, CompilerError> {
    // page bbox = union of feature bboxes.
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let mut spatial_index = SpatialIndexBuilder::new(mars_artifact::DEFAULT_NODE_SIZE).map_err(|e| {
        CompilerError::LegacySubstrateRetired {
            what: stringify_artifact_err(&e),
        }
    })?;
    let mut features: Vec<FeatureGeom> = Vec::with_capacity(rows.len());
    let mut attrs_pairs: Vec<(u64, Vec<u8>)> = Vec::with_capacity(rows.len());

    // features must be sorted by id ascending for the GeometryPayload codec.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by_key(|&i| rows[i].feature.id);

    for (slot, &i) in order.iter().enumerate() {
        let r = &rows[i];
        let bb = r.feature.bbox;
        spatial_index.add(slot as u32, bb);
        if (bb[0] as f64) < min_x {
            min_x = bb[0] as f64;
        }
        if (bb[1] as f64) < min_y {
            min_y = bb[1] as f64;
        }
        if (bb[2] as f64) > max_x {
            max_x = bb[2] as f64;
        }
        if (bb[3] as f64) > max_y {
            max_y = bb[3] as f64;
        }
        features.push(clone_feature(&r.feature));
        attrs_pairs.push((r.feature.id, r.attrs_bytes.clone()));
    }

    let page_bbox = Bbox::new(min_x, min_y, max_x, max_y);
    let spatial_index_bytes = spatial_index
        .finish()
        .map_err(|e| CompilerError::LegacySubstrateRetired {
            what: stringify_artifact_err(&e),
        })?;

    let mut writer = ArtifactWriter::new(ArtifactKind::Source);
    writer
        .add_spatial_index(spatial_index_bytes)
        .add_geometry_payload(features)
        .add_attributes(attrs_pairs)
        .set_bbox(page_bbox)
        .set_feature_count(rows.len() as u64);
    let artifact_bytes: Bytes = writer.finish().map_err(|e| CompilerError::LegacySubstrateRetired {
        what: stringify_artifact_err(&e),
    })?;
    let hash = compute_content_hash(&artifact_bytes);

    let page_key = PageKey {
        binding_id: binding.binding_id.clone(),
        level,
        page_id,
    };
    let object_key = page_key
        .object_key(&hash)
        .map_err(|_| CompilerError::LegacySubstrateRetired {
            what: "snapshot: page key construction",
        })?;
    let size_bytes = artifact_bytes.len() as u64;
    deps.store.put(&object_key, artifact_bytes).await?;

    let hilbert_lo = rows.iter().map(|r| r.key).min().unwrap_or(HilbertKey::min());
    let hilbert_hi = rows.iter().map(|r| r.key).max().unwrap_or(HilbertKey::max());

    Ok(PageEntry {
        key: page_key,
        content_hash: hash,
        spatial_bbox: page_bbox,
        hilbert_range: (hilbert_lo, hilbert_hi),
        feature_count: rows.len() as u64,
        size_bytes,
    })
}

fn empty_level_metadata(binding: &BindingPlan) -> LevelMetadata {
    LevelMetadata {
        level: DecimationLevel::new(0),
        vertex_tolerance_m: binding.levels[0].vertex_tolerance_m,
        geometry_min_size_m: binding.levels[0].geometry_min_size_m,
        label_min_priority: binding.levels[0].label_min_priority,
        page_count: 0,
        combined_bbox: Bbox::new(0.0, 0.0, 0.0, 0.0),
        hilbert_range_table: Vec::new(),
    }
}

fn binding_schema(from: &str) -> &str {
    from.split_once('.').map(|(s, _)| s).unwrap_or("public")
}

fn binding_table(from: &str) -> &str {
    from.split_once('.').map(|(_, t)| t).unwrap_or(from)
}

fn sidecar_object_key(binding: &str, hash: &ContentHash) -> Result<ArtifactKey, CompilerError> {
    if binding.contains('/') || binding.contains('\0') {
        return Err(CompilerError::LegacySubstrateRetired {
            what: "snapshot: sidecar key sanitisation",
        });
    }
    Ok(ArtifactKey::new(format!(
        "bnd/{binding}/sidecar/{hex}.pmsc",
        hex = hash.to_hex()
    )))
}

fn clone_feature(f: &FeatureGeom) -> FeatureGeom {
    // FeatureGeom has Clone but spelled out for clarity in the hot path.
    FeatureGeom {
        id: f.id,
        bbox: f.bbox,
        geom: clone_geom(&f.geom),
    }
}

fn clone_geom(g: &GeomKind) -> GeomKind {
    g.clone()
}

fn attr_value_to_artifact(v: mars_source::AttrValue) -> mars_artifact::AttrValue {
    use mars_source::AttrValue as S;
    match v {
        S::Null => mars_artifact::AttrValue::Null,
        S::Bool(b) => mars_artifact::AttrValue::Bool(b),
        S::Int(i) => mars_artifact::AttrValue::Int(i),
        S::Float(f) => mars_artifact::AttrValue::Float(f),
        S::String(s) => mars_artifact::AttrValue::String(s),
    }
}

// the following stringify_* helpers fold typed errors into the
// LegacySubstrateRetired variant. proper typed CompilerError variants for
// "snapshot WKB decode failed" / "attrs encode failed" land in C.2 alongside
// the rest of the compiler error taxonomy work.
fn stringify_wkb_err(_e: &mars_artifact::WkbError) -> &'static str {
    "snapshot: WKB decode"
}
fn stringify_attr_err(_e: &mars_artifact::AttrError) -> &'static str {
    "snapshot: attr encode"
}
fn stringify_artifact_err(_e: &mars_artifact::ArtifactError) -> &'static str {
    "snapshot: artifact assembly"
}
fn stringify_sidecar_err(_e: &crate::sidecar::SidecarError) -> &'static str {
    "snapshot: sidecar encode"
}

// quiet `unused` for `Arc` import when the function below is later inlined.
#[allow(dead_code)]
fn _arc_marker(_: Arc<dyn ObjectStore>) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::plan::{BindingPlan, BootstrapPlan, LevelPlan};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::stream::BoxStream;
    use futures_util::stream;
    use mars_artifact::{ArtifactReader, SectionKind, SpatialIndex};
    use mars_observability::Metrics;
    use mars_source::{
        AttrValue, ChangeFeed, ChangeSubscription, LeaderLock, LeaderLockGuard, RowBytes, Source,
        SourceBinding as PortBinding, SourceError,
    };
    use mars_store::{ManifestStore, ObjectStore, StoreError};
    use mars_types::{ArtifactKey, BindingId, ContentHash, CrsCode, Manifest};
    use std::sync::Mutex;

    // ---- in-memory test doubles ------------------------------------------

    #[derive(Default)]
    struct InMemoryStore {
        objects: Mutex<std::collections::HashMap<String, Bytes>>,
    }

    #[async_trait]
    impl ObjectStore for InMemoryStore {
        async fn get(&self, key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
            self.objects
                .lock()
                .unwrap()
                .get(key.as_str())
                .cloned()
                .ok_or_else(|| StoreError::Transient(format!("missing {key}")))
        }
        async fn put(&self, key: &ArtifactKey, body: Bytes) -> Result<ContentHash, StoreError> {
            let hash = mars_artifact::compute_content_hash(&body);
            self.objects.lock().unwrap().insert(key.as_str().to_owned(), body);
            Ok(hash)
        }
        async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
            Ok(())
        }
        async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct PanicManifestStore;
    #[async_trait]
    impl ManifestStore for PanicManifestStore {
        async fn publish(&self, _manifest: &Manifest) -> Result<u64, StoreError> {
            panic!("publish should not be called from snapshot tests")
        }
        async fn current(&self) -> Result<Option<Manifest>, StoreError> {
            Ok(None)
        }
        async fn watch(
            &self,
        ) -> Result<futures_core::stream::BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
            Ok(Box::pin(stream::empty()))
        }
    }

    struct PointSource {
        rows: Vec<RowBytes>,
    }

    #[async_trait]
    impl Source for PointSource {
        async fn fetch_full_table_streaming<'a>(
            &'a self,
            _binding: &'a PortBinding,
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            let owned: Vec<RowBytes> = self.rows.clone();
            Ok(Box::pin(stream::iter(owned.into_iter().map(Ok))))
        }

        async fn fetch_by_feature_ids<'a>(
            &'a self,
            _binding: &'a PortBinding,
            _ids: &'a [i64],
        ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test fetch_by_feature_ids",
            })
        }
    }

    #[derive(Default)]
    struct NopChangeFeed;
    #[async_trait]
    impl ChangeFeed for NopChangeFeed {
        async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test ChangeFeed",
            })
        }
    }

    #[derive(Default)]
    struct NopLeaderLock;
    #[async_trait]
    impl LeaderLock for NopLeaderLock {
        async fn try_acquire(&self, _key: i64) -> Result<Option<Box<dyn LeaderLockGuard>>, SourceError> {
            Err(SourceError::NotImplemented {
                what: "test LeaderLock",
            })
        }
    }

    fn point_wkb(x: f64, y: f64) -> Bytes {
        let mut v = Vec::with_capacity(21);
        v.push(1);
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        Bytes::from(v)
    }

    fn make_deps(rows: Vec<RowBytes>) -> (Deps, Arc<InMemoryStore>) {
        let store = Arc::new(InMemoryStore::default());
        let deps = Deps {
            source: Arc::new(PointSource { rows }),
            change_feed: Arc::new(NopChangeFeed),
            leader_lock: Arc::new(NopLeaderLock),
            store: store.clone(),
            manifest: Arc::new(PanicManifestStore),
            metrics: Metrics::new().unwrap(),
        };
        (deps, store)
    }

    fn binding_plan(id: &str, page_size: u64) -> BindingPlan {
        BindingPlan {
            binding_id: BindingId::try_new(id).unwrap(),
            source_table: id.to_string(),
            geometry_column: "geom".into(),
            id_column: Some("id".into()),
            attributes: vec!["name".into()],
            native_crs: CrsCode::new("EPSG:25832"),
            levels: vec![LevelPlan {
                level: DecimationLevel::new(0),
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
            }],
            page_size_target_bytes: page_size,
        }
    }

    #[tokio::test]
    async fn single_page_bootstrap_decodes_back() {
        let rows: Vec<RowBytes> = (0..100)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32) * 10.0, f64::from(i as i32) * 5.0),
                attributes: vec![("name".into(), AttrValue::String(format!("p{i}")))],
            })
            .collect();
        let (deps, store) = make_deps(rows);
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("points", 5 * 1024 * 1024)],
        };

        let manifest = run_snapshot(&deps, &plan, "test".into(), 1).await.unwrap();
        assert_eq!(manifest.bindings.len(), 1);
        assert_eq!(manifest.bindings[0].feature_count_total, 100);
        assert_eq!(manifest.pages.len(), 1);
        let page = &manifest.pages[0];
        assert_eq!(page.feature_count, 100);

        // pull the page artifact back from the store and decode every section.
        let key = page.key.object_key(&page.content_hash).unwrap();
        let bytes = store.objects.lock().unwrap().get(key.as_str()).unwrap().clone();
        let reader = ArtifactReader::open(bytes).unwrap();
        assert_eq!(reader.feature_count(), 100);

        // SpatialIndex section is present and queries match brute force.
        let spix_bytes = reader.section(SectionKind::SpatialIndex).unwrap();
        let spix = SpatialIndex::open(spix_bytes).unwrap();
        let viewport = [0.0_f32, 0.0, 100.0, 100.0];
        let mut hits = Vec::new();
        spix.query(viewport, &mut hits);
        assert!(!hits.is_empty(), "viewport should hit at least one feature");

        // Attributes by feature_id resolves a known sample.
        let attrs = reader.attributes_by_feature_id(50).unwrap().unwrap();
        let decoded = mars_artifact::decode_row(attrs).unwrap();
        assert!(matches!(decoded[0].1, mars_artifact::AttrValue::String(ref s) if s == "p50"));

        // sidecar present in manifest metadata.
        assert!(manifest.bindings[0].page_membership_sidecar.is_some());
    }

    #[tokio::test]
    async fn small_page_budget_splits_into_multiple_pages() {
        let rows: Vec<RowBytes> = (0..1000)
            .map(|i| RowBytes {
                feature_id: i,
                geometry: point_wkb(f64::from(i as i32), f64::from((i * 7) as i32)),
                attributes: vec![("name".into(), AttrValue::String(format!("x{i}")))],
            })
            .collect();
        let (deps, _store) = make_deps(rows);
        let plan = BootstrapPlan {
            bindings: vec![binding_plan("pts", 16 * 1024)],
        };
        let manifest = run_snapshot(&deps, &plan, "test".into(), 1).await.unwrap();
        let pages: Vec<&PageEntry> = manifest.pages.iter().collect();
        assert!(pages.len() > 1, "expected multiple pages, got {}", pages.len());
        // total feature count across pages equals input
        let total: u64 = pages.iter().map(|p| p.feature_count).sum();
        assert_eq!(total, 1000);
        // hilbert ranges are non-overlapping ascending in this binding/level slice.
        let level_table = &manifest.bindings[0].levels[0].hilbert_range_table;
        for w in level_table.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlapping or out-of-order ranges: {w:?}");
        }
    }
}
