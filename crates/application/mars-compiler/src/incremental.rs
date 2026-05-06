//! Pure helpers for the incremental compile loop: change-event lowering,
//! plan filtering, manifest merge. No I/O — exercised entirely from tests.

use std::collections::BTreeSet;
use std::time::SystemTime;

use mars_source::{ChangeBatch, ChangeEvent};
use mars_types::{ArtifactEntry, EmptyLayerCell, MANIFEST_FORMAT_VERSION, Manifest, ParsedArtifactKey};

use crate::plan::Plan;

/// Canonical key for a dirty source cell: `(collection, band, x, y)`.
pub type SourceCellKey = (String, String, i64, i64);

/// Set of `(collection, band, cell)` tuples that an incremental window has
/// invalidated. Deduplicated and ordered for stable iteration.
#[derive(Debug, Default, Clone)]
pub struct DirtySet {
    pub cells: BTreeSet<SourceCellKey>,
}

impl DirtySet {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Lower a window's worth of [`ChangeBatch`]es into the dirty source-cell
/// set, scoped to the configured plan. Truncate events expand to every
/// source cell the plan knows about for the named collection.
pub fn dirty_cells_for(batches: &[ChangeBatch], plan: &Plan) -> DirtySet {
    let mut out = DirtySet::default();
    for batch in batches {
        for ev in &batch.events {
            match ev {
                ChangeEvent::Insert { collection, cells }
                | ChangeEvent::Update { collection, cells }
                | ChangeEvent::Delete { collection, cells } => {
                    for c in cells {
                        out.cells
                            .insert((collection.clone(), c.band.as_str().to_string(), c.x, c.y));
                    }
                }
                ChangeEvent::Truncate { collection } => {
                    for s in &plan.sources {
                        if s.collection().as_str() == collection {
                            out.cells.insert((
                                collection.clone(),
                                s.band.as_str().to_string(),
                                s.cell.x,
                                s.cell.y,
                            ));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Restrict `plan` to the source tasks present in `dirty`, carrying every
/// dependent layer task with them. Source-task indices in the returned plan
/// are renumbered so [`Plan::dependents_by_source`] keeps working.
pub fn filter_plan(plan: &Plan, dirty: &DirtySet) -> Plan {
    let mut out = Plan::default();
    let mut idx_map: Vec<Option<usize>> = Vec::with_capacity(plan.sources.len());
    for s in &plan.sources {
        let key = (
            s.collection().as_str().to_string(),
            s.band.as_str().to_string(),
            s.cell.x,
            s.cell.y,
        );
        if dirty.cells.contains(&key) {
            idx_map.push(Some(out.sources.len()));
            out.sources.push(s.clone());
        } else {
            idx_map.push(None);
        }
    }
    for l in &plan.layers {
        if let Some(Some(new_idx)) = idx_map.get(l.source).copied() {
            let mut nl = l.clone();
            nl.source = new_idx;
            out.layers.push(nl);
        }
    }
    out
}

/// Merge the rebuild output from one window into the previous manifest.
///
/// Entries in the previous manifest that match a dirty source cell (or any
/// layer cell that depends on one) are dropped; fresh entries from `rebuild`
/// take their place. Empty-layer markers are merged the same way.
pub fn merge_manifest(
    prev: &Manifest,
    next_version: u64,
    service: &str,
    rebuild: crate::snapshot::SnapshotOutput,
    dirty: &DirtySet,
    source_version: Option<String>,
) -> Manifest {
    // dropped layer cells are derived: any prev layer entry whose (band, x, y)
    // pairs with a collection that is dirty for that (band, x, y) — but the
    // layer key alone does not carry collection. Resolve via the prev source
    // entries: build a map (collection, band, x, y) -> bool dirty.
    // For layers we conservatively treat as dirty any layer-cell that the new
    // rebuild also produces, plus any prev empty marker for the same cell.
    let dirty_layer_keys: BTreeSet<(String, String, i64, i64)> = rebuild
        .layer_artifacts
        .iter()
        .filter_map(|e| match e.key.parse() {
            Ok(ParsedArtifactKey::Layer { layer, cell }) => {
                Some((layer.as_str().to_string(), cell.band.as_str().to_string(), cell.x, cell.y))
            }
            _ => None,
        })
        .chain(rebuild.empty_layer_cells.iter().map(|m| {
            (
                m.layer.as_str().to_string(),
                m.cell.band.as_str().to_string(),
                m.cell.x,
                m.cell.y,
            )
        }))
        .collect();

    let keep_source: Vec<ArtifactEntry> = prev
        .source_artifacts
        .iter()
        .filter(|e| match e.key.parse() {
            Ok(ParsedArtifactKey::Source { collection, cell }) => !dirty.cells.contains(&(
                collection,
                cell.band.as_str().to_string(),
                cell.x,
                cell.y,
            )),
            _ => true,
        })
        .cloned()
        .collect();
    let keep_layer: Vec<ArtifactEntry> = prev
        .layer_artifacts
        .iter()
        .filter(|e| match e.key.parse() {
            Ok(ParsedArtifactKey::Layer { layer, cell }) => !dirty_layer_keys.contains(&(
                layer.as_str().to_string(),
                cell.band.as_str().to_string(),
                cell.x,
                cell.y,
            )),
            _ => true,
        })
        .cloned()
        .collect();
    let keep_empty: Vec<EmptyLayerCell> = prev
        .empty_layer_cells
        .iter()
        .filter(|m| {
            !dirty_layer_keys.contains(&(
                m.layer.as_str().to_string(),
                m.cell.band.as_str().to_string(),
                m.cell.x,
                m.cell.y,
            ))
        })
        .cloned()
        .collect();

    let mut source_artifacts = keep_source;
    source_artifacts.extend(rebuild.source_artifacts);
    let mut layer_artifacts = keep_layer;
    layer_artifacts.extend(rebuild.layer_artifacts);
    let mut empty_layer_cells = keep_empty;
    empty_layer_cells.extend(rebuild.empty_layer_cells);

    Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: next_version,
        service: service.to_string(),
        created_at: SystemTime::now(),
        source_artifacts,
        layer_artifacts,
        style_artifact: prev.style_artifact.clone(),
        empty_layer_cells,
        source_version,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    use mars_source::SourceBinding;
    use mars_source::SourceCollectionId;
    use mars_types::{ArtifactKey, Cell, ContentHash, CrsCode, LayerId, ScaleBand};

    use crate::class::CompiledClass;
    use crate::plan::{LayerTask, SourceTask};

    fn band(s: &str) -> ScaleBand {
        ScaleBand::new(s)
    }

    fn cell(band_s: &str, x: i64, y: i64) -> Cell {
        Cell {
            band: band(band_s),
            x,
            y,
        }
    }

    fn source_task(coll: &str, band_s: &str, x: i64, y: i64) -> SourceTask {
        SourceTask {
            band: band(band_s),
            cell: cell(band_s, x, y),
            binding: SourceBinding::new(
                SourceCollectionId::new(coll.to_string()),
                "public",
                coll,
                "geom",
                "id",
                vec![],
                CrsCode::new("EPSG:25832"),
            )
            .unwrap(),
            cell_size: 4096.0,
            origin: (0.0, 0.0),
        }
    }

    fn layer_task(layer: &str, band_s: &str, x: i64, y: i64, src: usize) -> LayerTask {
        LayerTask {
            layer: LayerId::new(layer),
            band: band(band_s),
            cell: cell(band_s, x, y),
            source: src,
            classes: Vec::<CompiledClass>::new(),
        }
    }

    fn entry(key: &str) -> ArtifactEntry {
        ArtifactEntry {
            key: ArtifactKey::new(key),
            hash: ContentHash::zero(),
            size_bytes: 0,
        }
    }

    #[test]
    fn dirty_cells_collect_per_event() {
        let plan = Plan::default();
        let batch = ChangeBatch {
            events: vec![
                ChangeEvent::Insert {
                    collection: "roads".into(),
                    cells: vec![cell("hi", 1, 2)],
                },
                ChangeEvent::Update {
                    collection: "roads".into(),
                    cells: vec![cell("hi", 1, 2), cell("hi", 3, 4)],
                },
            ],
            source_version: Some("0/100".into()),
        };
        let dirty = dirty_cells_for(&[batch], &plan);
        assert_eq!(dirty.cells.len(), 2);
        assert!(dirty.cells.contains(&("roads".into(), "hi".into(), 1, 2)));
        assert!(dirty.cells.contains(&("roads".into(), "hi".into(), 3, 4)));
    }

    #[test]
    fn truncate_expands_to_planned_cells_for_collection() {
        let mut plan = Plan::default();
        plan.sources.push(source_task("roads", "hi", 0, 0));
        plan.sources.push(source_task("roads", "hi", 1, 0));
        plan.sources.push(source_task("rivers", "hi", 5, 5));
        let batch = ChangeBatch {
            events: vec![ChangeEvent::Truncate {
                collection: "roads".into(),
            }],
            source_version: None,
        };
        let dirty = dirty_cells_for(&[batch], &plan);
        assert_eq!(dirty.cells.len(), 2);
        assert!(dirty.cells.contains(&("roads".into(), "hi".into(), 0, 0)));
        assert!(dirty.cells.contains(&("roads".into(), "hi".into(), 1, 0)));
    }

    #[test]
    fn filter_plan_keeps_dirty_sources_and_dependents() {
        let mut plan = Plan::default();
        plan.sources.push(source_task("a", "hi", 0, 0)); // idx 0 — dirty
        plan.sources.push(source_task("b", "hi", 0, 0)); // idx 1 — clean
        plan.layers.push(layer_task("l_a", "hi", 0, 0, 0));
        plan.layers.push(layer_task("l_b", "hi", 0, 0, 1));
        plan.layers.push(layer_task("l_a2", "hi", 0, 0, 0));

        let mut dirty = DirtySet::default();
        dirty.cells.insert(("a".into(), "hi".into(), 0, 0));

        let filtered = filter_plan(&plan, &dirty);
        assert_eq!(filtered.sources.len(), 1);
        assert_eq!(filtered.sources[0].collection().as_str(), "a");
        assert_eq!(filtered.layers.len(), 2);
        for l in &filtered.layers {
            assert_eq!(l.source, 0, "renumbered to point at the kept source");
            assert!(l.layer.as_str().starts_with("l_a"));
        }
    }

    #[test]
    fn merge_manifest_replaces_dirty_keeps_clean_bumps_version() {
        let prev = Manifest::new(
            5,
            "svc",
            vec![
                entry("src/a/hi/0_0/0000000000000000000000000000000000000000000000000000000000000000.mars"),
                entry("src/b/hi/0_0/0000000000000000000000000000000000000000000000000000000000000000.mars"),
            ],
            vec![
                entry("lyr/l_a/hi/0_0/v1/0000000000000000000000000000000000000000000000000000000000000000.mars"),
                entry("lyr/l_b/hi/0_0/v1/0000000000000000000000000000000000000000000000000000000000000000.mars"),
            ],
            None,
            vec![],
        );

        let rebuild = crate::snapshot::SnapshotOutput {
            source_artifacts: vec![entry(
                "src/a/hi/0_0/1111111111111111111111111111111111111111111111111111111111111111.mars",
            )],
            layer_artifacts: vec![entry(
                "lyr/l_a/hi/0_0/v1/2222222222222222222222222222222222222222222222222222222222222222.mars",
            )],
            empty_layer_cells: vec![],
        };

        let mut dirty = DirtySet::default();
        dirty.cells.insert(("a".into(), "hi".into(), 0, 0));

        let merged = merge_manifest(&prev, 6, "svc", rebuild, &dirty, Some("0/200".into()));
        assert_eq!(merged.version, 6);
        assert_eq!(merged.source_version.as_deref(), Some("0/200"));
        // b's source kept (clean), a's source replaced with the new hash
        assert_eq!(merged.source_artifacts.len(), 2);
        let src_keys: Vec<&str> = merged.source_artifacts.iter().map(|e| e.key.as_str()).collect();
        assert!(src_keys.iter().any(|k| k.contains("/b/")));
        assert!(src_keys.iter().any(|k| k.contains("/a/") && k.contains("1111")));
        // layers: l_b retained, l_a replaced
        assert_eq!(merged.layer_artifacts.len(), 2);
        let lyr_keys: Vec<&str> = merged.layer_artifacts.iter().map(|e| e.key.as_str()).collect();
        assert!(lyr_keys.iter().any(|k| k.contains("/l_b/")));
        assert!(lyr_keys.iter().any(|k| k.contains("/l_a/") && k.contains("2222")));
    }

    #[test]
    fn merge_manifest_handles_delete_to_empty() {
        // prev had a layer artifact for l_a@(0,0); rebuild emits an empty
        // marker for the same cell. result: no layer artifact, empty marker
        // present.
        let prev = Manifest::new(
            5,
            "svc",
            vec![entry(
                "src/a/hi/0_0/0000000000000000000000000000000000000000000000000000000000000000.mars",
            )],
            vec![entry(
                "lyr/l_a/hi/0_0/v1/0000000000000000000000000000000000000000000000000000000000000000.mars",
            )],
            None,
            vec![],
        );

        let rebuild = crate::snapshot::SnapshotOutput {
            source_artifacts: vec![],
            layer_artifacts: vec![],
            empty_layer_cells: vec![EmptyLayerCell {
                layer: LayerId::new("l_a"),
                cell: cell("hi", 0, 0),
            }],
        };

        let mut dirty = DirtySet::default();
        dirty.cells.insert(("a".into(), "hi".into(), 0, 0));

        let merged = merge_manifest(&prev, 6, "svc", rebuild, &dirty, None);
        assert!(merged.source_artifacts.is_empty(), "dirty source dropped");
        assert!(merged.layer_artifacts.is_empty(), "stale layer artifact dropped");
        assert_eq!(merged.empty_layer_cells.len(), 1);
    }
}
