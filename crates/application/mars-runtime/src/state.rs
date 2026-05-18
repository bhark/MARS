//! page-keyed runtime state.
//!
//! the manifest a snapshot ships is the on-disk source of truth, but the hot
//! path needs derived indices to avoid re-scanning the global `pages` vector
//! on every request. `PageIndex` is built once at manifest swap and pinned
//! for the lifetime of a [`RuntimeState`]; readers borrow into it.
//!
//! the manifest invariant is that `pages` is sorted by
//! `(binding_id, level, hilbert_range.0)` (see `mars_types::Manifest::pages`
//!). build-time validation re-checks the invariant and rejects
//! malformed manifests at swap time rather than at first request.

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
    /// two sidecars of the same kind reference the same `(layer, page_key)` -
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
    /// empty slice when the binding/level pair is not materialised - callers
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
/// `config` is the service config the manifest validated against - render
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
            let Some(from) = (match &source.kind {
                mars_config::BindingKind::PostgisTable { from, .. } => Some(from.as_str()),
                mars_config::BindingKind::PostgisSql { .. } | mars_config::BindingKind::Vectorfile { .. } => None,
            }) else {
                // sql: / vectorfile bindings carry hash-derived binding ids
                // and are not yet routable at the manifest level. skip the
                // manifest-membership probe so the layer can still appear in
                // capabilities while the snapshot path catches up.
                continue;
            };
            let id = match BindingId::try_new(from) {
                Ok(id) => id,
                // invalid binding id is a config-level error already enforced
                // by mars-config; we still fail closed here so a stale
                // manifest cannot mask a bad config.
                Err(e) => {
                    return Err(RuntimeError::ConfigManifestMismatch {
                        layer: layer.name.as_str().to_owned(),
                        reason: format!("source `from = \"{from}\"` is not a valid binding id: {e}"),
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
mod tests;
