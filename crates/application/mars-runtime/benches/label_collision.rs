//! label collision bench. drives `collide_and_emit_labels` directly with
//! synthetic `PreparedLabel` candidates so we get a clean signal on the
//! greedy O(n²) collision pass alone, without artifact decode or projection.
//!
//! groups:
//!  * `runtime_label_collision_count`     - fixed density, varying candidate count
//!  * `runtime_label_collision_density`   - fixed count, varying viewport density
//!  * `runtime_label_collision_placement` - all-Fixed / all-Auto / mixed
//!  * `runtime_label_collision_force`     - mix of FORCE-priority labels
//!
//! requires the `bench-internals` feature, which exposes the `pub(super)`
//! collision API via `mars_runtime::bench_internals`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_runtime::bench_internals::{
    PreparedLabel, PreparedPlacement, collide_and_emit_labels, new_position_candidate, new_prepared_label,
};
use mars_style::{AnchorPosition, Colour, LabelStyle};

const CANVAS_W: u32 = 1024;
const CANVAS_H: u32 = 1024;
/// label bbox half-extent. each candidate occupies a 16x16 px square
/// centred on its anchor.
const LABEL_HALF: f32 = 8.0;

#[derive(Clone, Copy)]
enum PlacementMix {
    AllFixed,
    AllAuto,
    Mixed,
}

#[derive(Clone, Copy)]
struct Recipe {
    n: usize,
    /// extent of the rectangle anchors are drawn from. smaller -> denser ->
    /// more collisions per pair.
    spread_px: f32,
    /// fraction of labels in [0.0, 1.0] flagged force=true.
    force_fraction: f32,
    placement: PlacementMix,
    /// candidate count for AUTO labels. fixed at 9 in the runtime; mirror
    /// here so the bench reflects production cost.
    auto_candidates: usize,
}

fn label_style(priority: u16, force: bool) -> Arc<LabelStyle> {
    Arc::new(LabelStyle {
        font_family: String::new(),
        font_size: 12.0,
        fill: Colour::rgba(0, 0, 0, 255),
        halo: None,
        priority,
        min_distance: 0.0,
        position: AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle_deg: None,
        partials: true,
        force,
    })
}

/// fast deterministic xorshift so the bench corpus is reproducible without
/// pulling in `rand`. seeded on `idx` so candidate i is identical between
/// runs.
fn rand01(idx: u64, salt: u64) -> f32 {
    let mut x = idx.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 30;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 27;
    ((x >> 33) as f32) / ((1u32 << 31) as f32)
}

fn build_corpus(r: Recipe) -> Vec<PreparedLabel> {
    let mut labels = Vec::with_capacity(r.n);
    let force_n = (r.force_fraction * r.n as f32).round() as usize;
    for i in 0..r.n {
        let cx = rand01(i as u64, 1) * r.spread_px;
        let cy = rand01(i as u64, 2) * r.spread_px;
        let bbox = (cx - LABEL_HALF, cy - LABEL_HALF, cx + LABEL_HALF, cy + LABEL_HALF);
        let priority = (rand01(i as u64, 3) * 1000.0) as u16;
        let force = i < force_n;
        let style = label_style(priority, force);

        let placement = match (r.placement, i % 2) {
            (PlacementMix::AllFixed, _) | (PlacementMix::Mixed, 0) => PreparedPlacement::Fixed {
                anchor_offset_px: (0.0, 0.0),
                bbox_px: bbox,
            },
            (PlacementMix::AllAuto, _) | (PlacementMix::Mixed, _) => {
                // synthesise N candidates around the anchor in the eight
                // cardinal/diagonal positions plus centre.
                let candidates = (0..r.auto_candidates)
                    .map(|k| {
                        let ang = (k as f32) * std::f32::consts::TAU / (r.auto_candidates as f32);
                        let dx = ang.cos() * LABEL_HALF * 2.0;
                        let dy = ang.sin() * LABEL_HALF * 2.0;
                        let bb = (
                            cx + dx - LABEL_HALF,
                            cy + dy - LABEL_HALF,
                            cx + dx + LABEL_HALF,
                            cy + dy + LABEL_HALF,
                        );
                        new_position_candidate((dx, dy), bb)
                    })
                    .collect();
                PreparedPlacement::Auto { candidates }
            }
        };

        labels.push(new_prepared_label(
            (cx, cy),
            String::new(),
            style,
            priority,
            0.0,
            placement,
        ));
    }
    labels
}

fn bench_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_label_collision_count");
    for &n in &[100usize, 500, 2000] {
        let recipe = Recipe {
            n,
            spread_px: 800.0, // moderate density
            force_fraction: 0.0,
            placement: PlacementMix::AllFixed,
            auto_candidates: 9,
        };
        let corpus = build_corpus(recipe);
        group.throughput(Throughput::Elements(n as u64));
        let id = BenchmarkId::from_parameter(format!("n_{n}"));
        group.bench_with_input(id, &corpus, |b, corpus| {
            b.iter_batched(
                || corpus.clone(),
                |labels| {
                    let ops = collide_and_emit_labels(labels, CANVAS_W, CANVAS_H);
                    black_box(ops);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_density(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_label_collision_density");
    let n = 1000;
    group.throughput(Throughput::Elements(n as u64));
    for (label, spread) in [("dense_200", 200.0_f32), ("medium_800", 800.0), ("sparse_3200", 3200.0)] {
        let recipe = Recipe {
            n,
            spread_px: spread,
            force_fraction: 0.0,
            placement: PlacementMix::AllFixed,
            auto_candidates: 9,
        };
        let corpus = build_corpus(recipe);
        let id = BenchmarkId::from_parameter(label);
        group.bench_with_input(id, &corpus, |b, corpus| {
            b.iter_batched(
                || corpus.clone(),
                |labels| {
                    let ops = collide_and_emit_labels(labels, CANVAS_W, CANVAS_H);
                    black_box(ops);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_placement(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_label_collision_placement");
    let n = 1000;
    group.throughput(Throughput::Elements(n as u64));
    for (label, mix) in [
        ("all_fixed", PlacementMix::AllFixed),
        ("all_auto", PlacementMix::AllAuto),
        ("mixed", PlacementMix::Mixed),
    ] {
        let recipe = Recipe {
            n,
            spread_px: 800.0,
            force_fraction: 0.0,
            placement: mix,
            auto_candidates: 9,
        };
        let corpus = build_corpus(recipe);
        let id = BenchmarkId::from_parameter(label);
        group.bench_with_input(id, &corpus, |b, corpus| {
            b.iter_batched(
                || corpus.clone(),
                |labels| {
                    let ops = collide_and_emit_labels(labels, CANVAS_W, CANVAS_H);
                    black_box(ops);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_force(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_label_collision_force");
    let n = 1000;
    group.throughput(Throughput::Elements(n as u64));
    for &frac in &[0.0_f32, 0.1, 0.5] {
        let recipe = Recipe {
            n,
            spread_px: 800.0,
            force_fraction: frac,
            placement: PlacementMix::AllFixed,
            auto_candidates: 9,
        };
        let corpus = build_corpus(recipe);
        let id = BenchmarkId::from_parameter(format!("force_{frac:.2}"));
        group.bench_with_input(id, &corpus, |b, corpus| {
            b.iter_batched(
                || corpus.clone(),
                |labels| {
                    let ops = collide_and_emit_labels(labels, CANVAS_W, CANVAS_H);
                    black_box(ops);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_count, bench_density, bench_placement, bench_force);
criterion_main!(benches);
