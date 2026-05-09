//! page-keyed runtime state.
//!
//! the manifest a snapshot ships is the on-disk source of truth, but the hot
//! path needs derived indices to avoid re-scanning the global `pages` vector
//! on every request. `PageIndex` is built once at manifest swap and pinned
//! for the lifetime of a [`RuntimeState`]; readers borrow into it.
//!
//! the manifest invariant is that `pages` is sorted by
//! `(binding_id, level, hilbert_range.0)` (LAZARUS §Manifest, mars-types
//! `Manifest::pages` doc). build-time validation re-checks the invariant and
//! rejects malformed manifests at swap time rather than at first request.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use mars_config::Config;
use mars_style::Stylesheet;
use mars_types::{
    BindingId, BindingMetadata, DecimationLevel, LayerId, LayerSidecarEntry, LayerSidecarKind, Manifest, PageEntry,
    PageKey,
};

use crate::RuntimeError;

/// derived view over a [`Manifest`]'s pages and per-layer sidecars.
///
/// indices are dense `usize` offsets into the manifest's collections; lookups
/// return borrows, so the manifest itself must outlive the index. this is
/// enforced by storing the index alongside the manifest inside
/// [`RuntimeState`].
#[derive(Debug, Default)]
pub struct PageIndex {
    /// per `(binding_id, level)`: contiguous `Range<usize>` into
    /// `manifest.pages`. binary-search by `(binding_id, level)` is O(1) here
    /// because the slice has already been computed at build time.
    page_slices: HashMap<(BindingId, DecimationLevel), Range<usize>>,
    /// per `binding_id`: index into `manifest.bindings` for native-crs / level
    /// rule access during the level-pick step.
    binding_index: HashMap<BindingId, usize>,
    /// per `(layer_id, page_key)`: index into `manifest.class_sidecars`.
    class_sidecar_index: HashMap<(LayerId, PageKey), usize>,
    /// per `(layer_id, page_key)`: index into `manifest.label_sidecars`.
    label_sidecar_index: HashMap<(LayerId, PageKey), usize>,
}

/// build-time errors raised when a manifest violates an invariant the runtime
/// relies on for hot-path correctness.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum IndexError {
    /// `manifest.pages` is not sorted by `(binding_id, level, hilbert_range.0)`.
    #[error(
        "manifest.pages is not sorted by (binding_id, level, hilbert_range.0); \
         offending entry at index {index}"
    )]
    PagesUnsorted {
        /// index of the first entry that breaks the invariant.
        index: usize,
    },
    /// a sidecar references a `PageKey` that is not present in the manifest.
    #[error("sidecar at index {index} references missing page {page_key:?}")]
    OrphanSidecar {
        /// index into `manifest.{class,label}_sidecars`.
        index: usize,
        /// kind of sidecar that orphaned.
        kind: LayerSidecarKind,
        /// page key the sidecar references but which has no matching `PageEntry`.
        page_key: PageKey,
    },
    /// two sidecars of the same kind reference the same `(layer, page_key)` —
    /// the manifest writer must dedupe before commit.
    #[error("duplicate {kind:?} sidecar for layer {layer} at {page_key:?}")]
    DuplicateSidecar {
        /// layer the duplicate references.
        layer: LayerId,
        /// page key the duplicate references.
        page_key: PageKey,
        /// kind of sidecar that duplicates.
        kind: LayerSidecarKind,
    },
}

impl PageIndex {
    /// produce the index for `manifest`. validates invariants the hot path
    /// relies on; failures surface as [`IndexError`] and the manifest is
    /// rejected at swap time.
    pub fn build(manifest: &Manifest) -> Result<Self, IndexError> {
        let page_slices = build_page_slices(&manifest.pages)?;
        let binding_index = manifest
            .bindings
            .iter()
            .enumerate()
            .map(|(i, b)| (b.binding_id.clone(), i))
            .collect();

        let valid_page_keys: HashSet<&PageKey> = manifest.pages.iter().map(|p| &p.key).collect();
        let class_sidecar_index =
            build_sidecar_index(&manifest.class_sidecars, LayerSidecarKind::Class, &valid_page_keys)?;
        let label_sidecar_index =
            build_sidecar_index(&manifest.label_sidecars, LayerSidecarKind::Label, &valid_page_keys)?;

        Ok(Self {
            page_slices,
            binding_index,
            class_sidecar_index,
            label_sidecar_index,
        })
    }

    /// borrow the contiguous page slice for `(binding_id, level)`. returns an
    /// empty slice when the binding/level pair is not materialised — callers
    /// treat that as "no candidate pages."
    #[must_use]
    pub fn page_slice<'m>(
        &self,
        manifest: &'m Manifest,
        binding_id: &BindingId,
        level: DecimationLevel,
    ) -> &'m [PageEntry] {
        match self.page_slices.get(&(binding_id.clone(), level)) {
            Some(range) => &manifest.pages[range.clone()],
            None => &[],
        }
    }

    /// borrow `BindingMetadata` for `binding_id` if present.
    #[must_use]
    pub fn binding<'m>(&self, manifest: &'m Manifest, binding_id: &BindingId) -> Option<&'m BindingMetadata> {
        self.binding_index.get(binding_id).map(|&i| &manifest.bindings[i])
    }

    /// borrow the class sidecar entry covering `(layer, page_key)` if any.
    #[must_use]
    pub fn class_sidecar<'m>(
        &self,
        manifest: &'m Manifest,
        layer: &LayerId,
        page_key: &PageKey,
    ) -> Option<&'m LayerSidecarEntry> {
        self.class_sidecar_index
            .get(&(layer.clone(), page_key.clone()))
            .map(|&i| &manifest.class_sidecars[i])
    }

    /// borrow the label sidecar entry covering `(layer, page_key)` if any.
    #[must_use]
    pub fn label_sidecar<'m>(
        &self,
        manifest: &'m Manifest,
        layer: &LayerId,
        page_key: &PageKey,
    ) -> Option<&'m LayerSidecarEntry> {
        self.label_sidecar_index
            .get(&(layer.clone(), page_key.clone()))
            .map(|&i| &manifest.label_sidecars[i])
    }

    /// total pages across all (binding, level) slices.
    #[must_use]
    pub fn total_pages(&self) -> usize {
        self.page_slices.values().map(|r| r.len()).sum()
    }

    /// number of bindings indexed.
    #[must_use]
    pub fn binding_count(&self) -> usize {
        self.binding_index.len()
    }
}

fn build_page_slices(pages: &[PageEntry]) -> Result<HashMap<(BindingId, DecimationLevel), Range<usize>>, IndexError> {
    let mut out: HashMap<(BindingId, DecimationLevel), Range<usize>> = HashMap::new();
    if pages.is_empty() {
        return Ok(out);
    }
    // single pass: validate the global ordering invariant and record
    // contiguous-run ranges per (binding, level).
    let mut run_start = 0usize;
    for i in 1..pages.len() {
        let prev = &pages[i - 1];
        let cur = &pages[i];
        let order = page_sort_key(prev).cmp(&page_sort_key(cur));
        if order == std::cmp::Ordering::Greater {
            return Err(IndexError::PagesUnsorted { index: i });
        }
        if (&prev.key.binding_id, prev.key.level) != (&cur.key.binding_id, cur.key.level) {
            out.insert((prev.key.binding_id.clone(), prev.key.level), run_start..i);
            run_start = i;
        }
    }
    let last = &pages[pages.len() - 1];
    out.insert((last.key.binding_id.clone(), last.key.level), run_start..pages.len());
    Ok(out)
}

fn page_sort_key(entry: &PageEntry) -> (&str, u8, u64) {
    (
        entry.key.binding_id.as_str(),
        entry.key.level.get(),
        entry.hilbert_range.0.get(),
    )
}

fn build_sidecar_index(
    sidecars: &[LayerSidecarEntry],
    kind: LayerSidecarKind,
    valid_page_keys: &HashSet<&PageKey>,
) -> Result<HashMap<(LayerId, PageKey), usize>, IndexError> {
    let mut out: HashMap<(LayerId, PageKey), usize> = HashMap::with_capacity(sidecars.len());
    for (i, sc) in sidecars.iter().enumerate() {
        if !valid_page_keys.contains(&sc.page_key) {
            return Err(IndexError::OrphanSidecar {
                index: i,
                kind,
                page_key: sc.page_key.clone(),
            });
        }
        if out.insert((sc.layer_id.clone(), sc.page_key.clone()), i).is_some() {
            return Err(IndexError::DuplicateSidecar {
                layer: sc.layer_id.clone(),
                page_key: sc.page_key.clone(),
                kind,
            });
        }
    }
    Ok(out)
}

/// loaded snapshot of the active manifest plus the derived [`PageIndex`].
///
/// the index is the hot-path-friendly view of the same manifest; readers that
/// only need the on-disk fields can reach for `state.manifest` directly. the
/// `config` is the service config the manifest validated against — render
/// uses it to look up per-layer bindings, classes, and label policy.
#[derive(Debug)]
pub struct RuntimeState {
    /// the active manifest snapshot.
    pub manifest: Manifest,
    /// active stylesheet (passed through unchanged).
    pub stylesheet: Stylesheet,
    /// service config the manifest was validated against. `None` only on
    /// `RuntimeState::empty` (readiness stand-ins); render uses
    /// [`Self::config_or_err`] to fail closed when absent.
    pub config: Option<Arc<Config>>,
    /// derived view over `manifest` pre-computed at swap time.
    pub index: PageIndex,
}

impl RuntimeState {
    /// build a runtime state from the current config, stylesheet, and a
    /// freshly-loaded manifest. validates that every layer in the config
    /// resolves to ≥1 binding present in the manifest; rejects malformed
    /// manifests via [`IndexError`].
    pub fn from_config_and_manifest(
        config: &Config,
        stylesheet: Stylesheet,
        manifest: Manifest,
    ) -> Result<Self, RuntimeError> {
        let index = PageIndex::build(&manifest).map_err(|source| RuntimeError::InvalidManifest {
            reason: source.to_string(),
        })?;
        validate_config_against_manifest(config, &manifest, &index)?;
        Ok(Self {
            manifest,
            stylesheet,
            config: Some(Arc::new(config.clone())),
            index,
        })
    }

    /// build the smallest runtime state that satisfies `is_ready()`. used by
    /// http-layer tests and any code that needs a manifest-empty stand-in.
    /// renders against this state fail with `RuntimeError::NotReady` because
    /// no config is present.
    #[must_use]
    pub fn empty(version: u64, service: impl Into<String>) -> Self {
        let manifest = Manifest::empty(version, service);
        let index = PageIndex::default();
        Self {
            manifest,
            stylesheet: Stylesheet::default(),
            config: None,
            index,
        }
    }

    /// borrow the active config or return `RuntimeError::NotReady`. the empty
    /// state has no config; production paths always do.
    pub fn config_or_err(&self) -> Result<&Config, RuntimeError> {
        self.config.as_deref().ok_or(RuntimeError::NotReady)
    }
}

fn validate_config_against_manifest(
    config: &Config,
    manifest: &Manifest,
    index: &PageIndex,
) -> Result<(), RuntimeError> {
    // layers without sources cannot resolve to anything; flag distinctly.
    for layer in &config.layers {
        if layer.sources.is_empty() {
            return Err(RuntimeError::ConfigManifestMismatch {
                layer: layer.name.as_str().to_owned(),
                reason: "layer declares zero sources".to_owned(),
            });
        }
        let mut any_in_manifest = false;
        for source in &layer.sources {
            let id = match BindingId::try_new(source.from.as_str()) {
                Ok(id) => id,
                // invalid binding id is a config-level error already enforced
                // by mars-config; we still fail closed here so a stale
                // manifest cannot mask a bad config.
                Err(e) => {
                    return Err(RuntimeError::ConfigManifestMismatch {
                        layer: layer.name.as_str().to_owned(),
                        reason: format!("source `from = \"{}\"` is not a valid binding id: {e}", source.from),
                    });
                }
            };
            if index.binding(manifest, &id).is_some() {
                any_in_manifest = true;
                break;
            }
        }
        if !any_in_manifest {
            return Err(RuntimeError::ConfigManifestMismatch {
                layer: layer.name.as_str().to_owned(),
                reason: format!(
                    "no source binding for layer is present in manifest v{}",
                    manifest.version
                ),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::SystemTime;

    use mars_types::{Bbox, ContentHash, HilbertKey, LayerId, MANIFEST_FORMAT_VERSION, PageId};

    use super::*;

    fn page(binding: &str, level: u8, hilbert_lo: u64, page_id: u64) -> PageEntry {
        PageEntry {
            key: PageKey {
                binding_id: BindingId::try_new(binding).unwrap(),
                level: DecimationLevel::new(level),
                page_id: PageId::new(page_id),
            },
            content_hash: ContentHash::zero(),
            spatial_bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            hilbert_range: (HilbertKey::new(hilbert_lo), HilbertKey::new(hilbert_lo + 1)),
            feature_count: 0,
            size_bytes: 0,
        }
    }

    fn sidecar(layer: &str, page_key: PageKey, kind: LayerSidecarKind) -> LayerSidecarEntry {
        LayerSidecarEntry {
            layer_id: LayerId::new(layer),
            page_key,
            content_hash: ContentHash::zero(),
            size_bytes: 0,
            kind,
        }
    }

    fn manifest_with(pages: Vec<PageEntry>, bindings: Vec<BindingMetadata>) -> Manifest {
        Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            version: 1,
            service: "test".into(),
            created_at: SystemTime::UNIX_EPOCH,
            bindings,
            pages,
            class_sidecars: vec![],
            label_sidecars: vec![],
            style_artifact: None,
            source_version: None,
            epoch: 0,
        }
    }

    #[test]
    fn slices_are_contiguous_per_binding_level() {
        let pages = vec![
            page("a", 0, 0, 1),
            page("a", 0, 10, 2),
            page("a", 1, 0, 3),
            page("b", 0, 0, 4),
        ];
        let m = manifest_with(pages, vec![]);
        let idx = PageIndex::build(&m).unwrap();
        assert_eq!(idx.total_pages(), 4);
        assert_eq!(
            idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(0))
                .len(),
            2
        );
        assert_eq!(
            idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(1))
                .len(),
            1
        );
        assert_eq!(
            idx.page_slice(&m, &BindingId::try_new("b").unwrap(), DecimationLevel::new(0))
                .len(),
            1
        );
    }

    fn assert_unsorted_at(pages: Vec<PageEntry>, expected: usize) {
        let m = manifest_with(pages, vec![]);
        match PageIndex::build(&m) {
            Err(IndexError::PagesUnsorted { index }) => assert_eq!(index, expected),
            other => panic!("expected PagesUnsorted at {expected}, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unsorted_pages() {
        assert_unsorted_at(vec![page("b", 0, 0, 1), page("a", 0, 0, 2)], 1);
    }

    #[test]
    fn rejects_unsorted_levels_within_binding() {
        assert_unsorted_at(vec![page("a", 1, 0, 1), page("a", 0, 0, 2)], 1);
    }

    #[test]
    fn rejects_unsorted_hilbert_within_level() {
        assert_unsorted_at(vec![page("a", 0, 10, 1), page("a", 0, 0, 2)], 1);
    }

    #[test]
    fn missing_binding_level_returns_empty_slice() {
        let pages = vec![page("a", 0, 0, 1)];
        let m = manifest_with(pages, vec![]);
        let idx = PageIndex::build(&m).unwrap();
        assert!(
            idx.page_slice(&m, &BindingId::try_new("a").unwrap(), DecimationLevel::new(7))
                .is_empty()
        );
        assert!(
            idx.page_slice(&m, &BindingId::try_new("missing").unwrap(), DecimationLevel::new(0))
                .is_empty()
        );
    }

    #[test]
    fn orphan_class_sidecar_rejected() {
        let pages = vec![page("a", 0, 0, 1)];
        let mut m = manifest_with(pages, vec![]);
        let orphan_key = PageKey {
            binding_id: BindingId::try_new("ghost").unwrap(),
            level: DecimationLevel::new(0),
            page_id: PageId::new(99),
        };
        m.class_sidecars
            .push(sidecar("layer-a", orphan_key, LayerSidecarKind::Class));
        match PageIndex::build(&m) {
            Err(IndexError::OrphanSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Class),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn orphan_sidecar_with_valid_binding_level_but_unknown_page_id_rejected() {
        // (binding, level) bucket exists but the specific page_id does not —
        // a coarse bucket-only check would let this stale sidecar survive.
        let pages = vec![page("a", 0, 0, 1)];
        let mut m = manifest_with(pages.clone(), vec![]);
        let stale = PageKey {
            binding_id: pages[0].key.binding_id.clone(),
            level: pages[0].key.level,
            page_id: PageId::new(pages[0].key.page_id.get() + 999),
        };
        m.class_sidecars
            .push(sidecar("layer-a", stale, LayerSidecarKind::Class));
        match PageIndex::build(&m) {
            Err(IndexError::OrphanSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Class),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn duplicate_sidecar_rejected() {
        let pages = vec![page("a", 0, 0, 1)];
        let mut m = manifest_with(pages.clone(), vec![]);
        let key = pages[0].key.clone();
        m.label_sidecars
            .push(sidecar("layer-a", key.clone(), LayerSidecarKind::Label));
        m.label_sidecars.push(sidecar("layer-a", key, LayerSidecarKind::Label));
        match PageIndex::build(&m) {
            Err(IndexError::DuplicateSidecar { kind, .. }) => assert_eq!(kind, LayerSidecarKind::Label),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn empty_manifest_yields_empty_index() {
        let m = Manifest::empty(1, "svc");
        let idx = PageIndex::build(&m).unwrap();
        assert_eq!(idx.total_pages(), 0);
        assert_eq!(idx.binding_count(), 0);
    }
}
