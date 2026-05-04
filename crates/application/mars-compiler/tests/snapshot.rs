#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support {
    pub(crate) mod mem_source;
}

use std::collections::BTreeMap;
use std::sync::Arc;

use mars_artifact::{ArtifactReader, SectionKind, decode_class_assignment};
use mars_compiler::{Compiler, Deps};
use mars_config::{
    ArtifactCache, ArtifactStore, Artifacts, Cells, Class, ClassStyle, Config, Interfaces, Layer, Scales, ServiceMeta,
    Source as CfgSource, SourceBinding as CfgBinding, model::Band,
};
use mars_source::{AttrValue, RowBytes, SourceCollectionId};
use mars_store::{ManifestStore, ObjectStore};
use mars_store::mem::{InMemoryPublisher, InMemoryStore};
use mars_types::{Bbox, Cell, CrsCode, LayerId, ScaleBand};
use tokio_util::sync::CancellationToken;

use crate::support::mem_source::{MemSource, wkb_polygon};

fn make_config() -> Config {
    let mut size_per_band = BTreeMap::new();
    size_per_band.insert("hi".to_string(), "4096m".to_string());

    Config {
        service: ServiceMeta {
            name: "test_svc".to_string(),
            ..Default::default()
        },
        source: CfgSource {
            kind: "memory".to_string(),
            dsn: "memory://".to_string(),
            native_crs: CrsCode::new("EPSG:25832"),
            change_feed: None,
        },
        artifacts: Artifacts {
            store: ArtifactStore {
                kind: "fs".to_string(),
                endpoint: None,
                bucket: None,
                prefix: None,
                path: None,
            },
            cache: ArtifactCache {
                path: "/tmp".to_string(),
                max_size: "1MiB".to_string(),
                eviction: "lru".to_string(),
            },
        },
        scales: Scales {
            bands: vec![Band {
                name: "hi".to_string(),
                max_denom: 25_000,
            }],
        },
        cells: Cells {
            grid: "regular".to_string(),
            origin: [0.0, 0.0],
            size_per_band,
            // single 4096m cell at origin (0,0)
            extent: Some(Bbox::new(0.0, 0.0, 1.0, 1.0)),
        },
        interfaces: Interfaces::default(),
        tile_matrix_sets: Default::default(),
        reprojection: Default::default(),
        styles: Default::default(),
        layers: vec![Layer {
            name: LayerId::new("roads"),
            title: String::new(),
            abstract_: String::new(),
            kind: "polygon".to_string(),
            scale: None,
            group: None,
            enable_get_feature_info: false,
            bbox: None,
            sources: vec![CfgBinding {
                scale: None,
                band: Some("hi".to_string()),
                from: "public.roads".to_string(),
                geometry_column: "geom".to_string(),
                id_column: Some("gid".to_string()),
                attributes: vec!["attr".to_string()],
            }],
            classes: vec![
                Class {
                    name: "a".to_string(),
                    title: String::new(),
                    when: Some("attr = 'a'".to_string()),
                    style: ClassStyle::Ref {
                        name: "style_a".to_string(),
                    },
                },
                Class {
                    name: "b".to_string(),
                    title: String::new(),
                    when: Some("attr = 'b'".to_string()),
                    style: ClassStyle::Ref {
                        name: "style_b".to_string(),
                    },
                },
            ],
            label: None,
        }],
        observability: Default::default(),
    }
}

fn make_rows() -> Vec<RowBytes> {
    let mut out = Vec::new();
    let polys: [(u64, &str); 5] = [(1, "a"), (2, "a"), (3, "a"), (4, "b"), (5, "b")];
    for (id, val) in polys {
        let dx = (id as f64) * 10.0;
        let coords = [(dx, dx), (dx + 5.0, dx), (dx + 5.0, dx + 5.0), (dx, dx + 5.0), (dx, dx)];
        out.push(RowBytes {
            feature_id: id,
            geometry: wkb_polygon(&coords),
            attributes: vec![("attr".to_string(), AttrValue::String(val.to_string()))],
        });
    }
    out
}

fn make_rows_with_unmatched_class() -> Vec<RowBytes> {
    let mut rows = make_rows();
    let coords = [(60.0, 60.0), (65.0, 60.0), (65.0, 65.0), (60.0, 65.0), (60.0, 60.0)];
    rows.push(RowBytes {
        feature_id: 6,
        geometry: wkb_polygon(&coords),
        attributes: vec![("attr".to_string(), AttrValue::String("c".to_string()))],
    });
    rows
}

fn build_deps(rows: Vec<RowBytes>) -> (Deps, Arc<InMemoryStore>, Arc<InMemoryPublisher>) {
    let store = Arc::new(InMemoryStore::new());
    let publisher = Arc::new(InMemoryPublisher::new());

    let mut mem = MemSource::default();
    mem.insert(
        SourceCollectionId::new("public.roads"),
        Cell {
            band: ScaleBand::new("hi"),
            x: 0,
            y: 0,
        },
        rows,
    );
    let mem = Arc::new(mem);

    let deps = Deps {
        source: mem.clone() as Arc<dyn mars_source::Source>,
        change_feed: mem as Arc<dyn mars_source::ChangeFeed>,
        store: store.clone() as Arc<dyn ObjectStore>,
        manifest: publisher.clone() as Arc<dyn ManifestStore>,
    };
    (deps, store, publisher)
}

#[tokio::test]
async fn snapshot_writes_artifacts_and_publishes_manifest() {
    let cfg = make_config();
    let (deps, store, publisher) = build_deps(make_rows());
    let compiler = Compiler::new(deps, cfg);
    compiler.run(CancellationToken::new()).await.unwrap();

    let src_keys = store.list("src").await.unwrap();
    assert_eq!(src_keys.len(), 1, "one source artifact: {src_keys:?}");
    let lyr_keys = store.list("lyr").await.unwrap();
    assert_eq!(lyr_keys.len(), 1, "one layer artifact: {lyr_keys:?}");

    assert!(src_keys[0].as_str().starts_with("src/public.roads/hi/0_0/"));
    assert!(lyr_keys[0].as_str().starts_with("lyr/roads/hi/0_0/v1/"));

    let manifest = publisher.current().await.unwrap().unwrap();
    assert_eq!(manifest.version, 1);

    // open the layer artifact and verify class_assignment
    let lyr_entry = &manifest.layer_artifacts[0];
    let bytes = store.get(&lyr_entry.key, lyr_entry.hash).await.unwrap();
    let reader = ArtifactReader::open(bytes).unwrap();
    let payload = reader.section(SectionKind::ClassAssignment).unwrap();
    let assigns = decode_class_assignment(&payload).unwrap();
    assert_eq!(
        assigns,
        vec![(1, 0u16), (2, 0), (3, 0), (4, 1), (5, 1)],
        "first-match-wins assignment in id order",
    );
}

#[tokio::test]
async fn snapshot_omits_unmatched_rows_from_layer_assignment() {
    let cfg = make_config();
    let (deps, store, publisher) = build_deps(make_rows_with_unmatched_class());
    let compiler = Compiler::new(deps, cfg);
    compiler.run(CancellationToken::new()).await.unwrap();

    let manifest = publisher.current().await.unwrap().unwrap();
    let lyr_entry = &manifest.layer_artifacts[0];
    let bytes = store.get(&lyr_entry.key, lyr_entry.hash).await.unwrap();
    let reader = ArtifactReader::open(bytes).unwrap();
    let payload = reader.section(SectionKind::ClassAssignment).unwrap();
    let assigns = decode_class_assignment(&payload).unwrap();

    assert_eq!(assigns, vec![(1, 0u16), (2, 0), (3, 0), (4, 1), (5, 1)]);
    assert_eq!(reader.feature_count(), 5);
}

#[tokio::test]
async fn snapshot_is_deterministic() {
    let cfg = make_config();

    let (deps1, store1, _) = build_deps(make_rows());
    Compiler::new(deps1, cfg.clone())
        .run(CancellationToken::new())
        .await
        .unwrap();
    let lyr1 = store1.list("lyr").await.unwrap();
    let src1 = store1.list("src").await.unwrap();

    let (deps2, store2, _) = build_deps(make_rows());
    Compiler::new(deps2, cfg).run(CancellationToken::new()).await.unwrap();
    let lyr2 = store2.list("lyr").await.unwrap();
    let src2 = store2.list("src").await.unwrap();

    assert_eq!(lyr1, lyr2, "layer keys (= content hashes) must match");
    assert_eq!(src1, src2, "source keys (= content hashes) must match");
}
