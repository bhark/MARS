//! manifest swap bench. measures the two synchronous halves of the reload
//! path that `run_manifest_reload_loop` would drive in production:
//!
//!  1. `RuntimeState::from_config_and_manifest` - manifest validation +
//!     PageIndex construction. cost grows with (binding count × pages).
//!  2. `Runtime::swap_state` - atomic ArcSwap publish + observer notify.
//!     should be near-constant; deviation here surfaces lock contention.
//!
//! manifests are synthesised on top of a `build_multi_layer_fixture` base by
//! cloning page entries within each binding to inflate `pages_per_binding`.
//! the synthetic page bytes are never fetched (state build doesn't touch
//! the object store), so we reuse the fixture's content_hash for every
//! synthetic entry.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::sync::Arc;
use std::time::SystemTime;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_runtime::test_fixtures::{
    build_minimal_stylesheet, build_multi_layer_config, build_multi_layer_fixture, build_multi_layer_stylesheet,
};
use mars_runtime::{Runtime, RuntimeState};
use mars_style::Stylesheet;
use mars_types::{
    BindingId, BindingMetadata, DecimationLevel, HilbertKey, LayerId, MANIFEST_FORMAT_VERSION, Manifest, PageEntry,
    PageId, PageKey,
};

const N_BINDINGS: &[usize] = &[1, 4, 16];
const PAGES_PER_BINDING: &[u64] = &[1, 100, 1000];

/// build a synthetic manifest with `n_bindings` × `pages_per_binding` pages.
/// reuses the multi-layer fixture's binding ids (so the config still
/// validates) and stamps extra pages with synthetic page ids and an
/// ascending hilbert range so the global sort invariant holds.
fn synth_manifest(base: &Manifest, base_bindings: &[BindingId], pages_per_binding: u64) -> Manifest {
    let mut pages = Vec::with_capacity(base_bindings.len() * pages_per_binding as usize);
    let mut bindings_meta = Vec::with_capacity(base_bindings.len());

    for (binding_idx, binding_id) in base_bindings.iter().enumerate() {
        // template page from the base fixture: per-binding page 0.
        let template = base
            .pages
            .iter()
            .find(|p| &p.key.binding_id == binding_id)
            .expect("base manifest must contain at least one page per binding");

        let mut hilbert_table = Vec::with_capacity(pages_per_binding as usize);
        let stride: u64 = u64::MAX / pages_per_binding.max(1);
        for i in 0..pages_per_binding {
            let pid = PageId::new(i + 1);
            let h_lo = HilbertKey::new(i.saturating_mul(stride));
            let h_hi = HilbertKey::new(((i + 1).saturating_mul(stride)).saturating_sub(1));
            hilbert_table.push((h_lo, h_hi, pid));
            pages.push(PageEntry {
                key: PageKey {
                    binding_id: binding_id.clone(),
                    level: DecimationLevel::new(0),
                    page_id: pid,
                },
                content_hash: template.content_hash,
                spatial_bbox: template.spatial_bbox,
                hilbert_range: (h_lo, h_hi),
                feature_count: template.feature_count,
                size_bytes: template.size_bytes,
            });
        }

        bindings_meta.push(BindingMetadata {
            binding_id: binding_id.clone(),
            source_table: format!("public.layer_{binding_idx}"),
            native_crs: base.bindings[binding_idx].native_crs.clone(),
            feature_count_total: pages_per_binding,
            combined_bbox: template.spatial_bbox,
            levels: vec![mars_types::LevelMetadata {
                level: DecimationLevel::new(0),
                vertex_tolerance_m: 0.0,
                geometry_min_size_m: 0.0,
                label_min_priority: 0,
                page_count: u32::try_from(pages_per_binding).unwrap_or(u32::MAX),
                hilbert_range_table: hilbert_table,
            }],
            page_membership_sidecar: None,
            cycles_since_reconcile: 0,
            last_reconcile_at: None,
        });
    }

    // global sort invariant: pages must be ordered by
    // (binding_id, level, hilbert_range.0). binding_ids like "binding_10"
    // sort before "binding_2" lexicographically, so the numerical loop
    // above produces a globally-unsorted vec; fix it in one pass here.
    pages.sort_by(|a, b| {
        a.key
            .binding_id
            .cmp(&b.key.binding_id)
            .then(a.key.level.cmp(&b.key.level))
            .then(a.hilbert_range.0.cmp(&b.hilbert_range.0))
    });

    Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        version: base.version + 1,
        service: base.service.clone(),
        created_at: SystemTime::UNIX_EPOCH,
        bindings: bindings_meta,
        pages,
        // sidecars elided: state build only orphan-checks them, so leaving
        // them empty stays valid and isolates the page-index build cost.
        class_sidecars: Vec::new(),
        label_sidecars: Vec::new(),
        style_artifact: None,
        image_artifact: None,
        raster_layers: Vec::new(),
        source_version: None,
        epoch: 0,
    }
}

/// snapshot the inputs `from_config_and_manifest` needs for one `n_bindings`
/// fixture: config, stylesheet, base manifest, binding-id list.
struct SwapInputs {
    config: mars_config::model::Config,
    stylesheet: Stylesheet,
    base_manifest: Manifest,
    binding_ids: Vec<BindingId>,
    layer_ids: Vec<LayerId>,
}

fn build_inputs(rt: &tokio::runtime::Runtime, n_bindings: usize) -> SwapInputs {
    let fix = rt.block_on(build_multi_layer_fixture(n_bindings, |store, _| store));
    let layer_ids = fix.layer_ids.clone();
    let binding_ids: Vec<BindingId> = (0..n_bindings)
        .map(|i| BindingId::try_new(format!("binding_{i}")).unwrap())
        .collect();
    let config = build_multi_layer_config(&layer_ids, &binding_ids);
    let stylesheet = if n_bindings == 1 {
        build_minimal_stylesheet()
    } else {
        build_multi_layer_stylesheet(n_bindings)
    };
    let base_manifest = fix.runtime.current_state().expect("ready").manifest.clone();
    SwapInputs {
        config,
        stylesheet,
        base_manifest,
        binding_ids,
        layer_ids,
    }
}

fn bench_state_build(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("runtime_manifest_state_build");
    for &n_bindings in N_BINDINGS {
        let inputs = build_inputs(&rt, n_bindings);
        for &ppb in PAGES_PER_BINDING {
            let manifest = synth_manifest(&inputs.base_manifest, &inputs.binding_ids, ppb);
            let total_pages = manifest.pages.len();
            group.throughput(Throughput::Elements(total_pages as u64));
            let id = BenchmarkId::from_parameter(format!("bindings_{n_bindings}_pages_{total_pages}"));
            group.bench_with_input(
                id,
                &(inputs.config.clone(), inputs.stylesheet.clone(), manifest),
                |b, (cfg, ss, mf)| {
                    b.iter_batched(
                        || (cfg.clone(), ss.clone(), mf.clone()),
                        |(cfg, ss, mf)| {
                            let state = RuntimeState::from_config_and_manifest(&cfg, ss, mf).unwrap();
                            black_box(state);
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
        // touch unused fields so they don't dead-code-warn.
        let _ = &inputs.layer_ids;
    }
    group.finish();
}

fn bench_swap_state(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut group = c.benchmark_group("runtime_manifest_swap_state");
    for &n_bindings in N_BINDINGS {
        let inputs = build_inputs(&rt, n_bindings);
        for &ppb in PAGES_PER_BINDING {
            let manifest = synth_manifest(&inputs.base_manifest, &inputs.binding_ids, ppb);
            let total_pages = manifest.pages.len();
            // pre-build the state once; swap_state is what we measure.
            let state = Arc::new(
                RuntimeState::from_config_and_manifest(&inputs.config, inputs.stylesheet.clone(), manifest).unwrap(),
            );
            let fix = rt.block_on(build_multi_layer_fixture(n_bindings, |store, _| store));
            let runtime: Arc<Runtime> = fix.runtime.clone();

            group.throughput(Throughput::Elements(total_pages as u64));
            let id = BenchmarkId::from_parameter(format!("bindings_{n_bindings}_pages_{total_pages}"));
            group.bench_function(id, |b| {
                b.iter(|| {
                    runtime.swap_state(state.clone());
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_state_build, bench_swap_state);
criterion_main!(benches);
