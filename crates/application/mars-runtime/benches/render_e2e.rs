//! end-to-end render bench for `Runtime::render`.
//!
//! drives the composite path: viewport intersect -> per-layer page fetch
//! (warm in-memory cache) -> ArtifactReader::open + spatial query +
//! geometry decode -> class join + style lookup -> projection (same- or
//! cross-CRS) -> label collision -> renderer paint -> encode.
//!
//! three groups:
//!  * `runtime_render_e2e_layer_scaling`   - vary layer count (orchestration)
//!  * `runtime_render_e2e_feature_density` - vary feature count per page
//!  * `runtime_render_e2e_crs`             - same-CRS vs cross-CRS request
//!
//! all groups use the in-memory fixtures from `mars_runtime::test_fixtures`;
//! object store + cache + renderer + encoder are all port-level stand-ins so
//! the bench exercises the runtime's own code path, not adapter overhead.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mars_runtime::test_fixtures::{FixtureOptions, REQUEST_CRS, build_fixture_with, build_multi_layer_fixture};
use mars_runtime::{OGC_STANDARDIZED_PIXEL_SIZE_M, RenderPlan};
use mars_style::LabelSurvival;
use mars_types::{Bbox, CrsCode, ImageFormat};

const CANVAS_W: u32 = 512;
const CANVAS_H: u32 = 512;

fn pixel_throughput() -> Throughput {
    // four bytes per pixel through the renderer; matches feature_prep's
    // throughput convention so signals stay comparable.
    Throughput::Bytes(u64::from(CANVAS_W) * u64::from(CANVAS_H) * 4)
}

fn tokio_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn bench_layer_scaling(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("runtime_render_e2e_layer_scaling");
    group.throughput(pixel_throughput());
    for &n_layers in &[1usize, 2, 4] {
        let fixture = rt.block_on(build_multi_layer_fixture(n_layers, |store, _| store));
        // widen plan to encompass every layer's 10x10 cell.
        let mut plan = fixture.render_plan();
        plan.width = CANVAS_W;
        plan.height = CANVAS_H;

        let id = BenchmarkId::from_parameter(format!("layers_{n_layers}"));
        group.bench_with_input(id, &(fixture.runtime.clone(), plan), |b, (runtime, plan)| {
            b.iter(|| {
                let bytes = rt.block_on(runtime.render(plan)).unwrap();
                black_box(bytes);
            });
        });
    }
    group.finish();
}

fn bench_feature_density(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("runtime_render_e2e_feature_density");
    group.throughput(pixel_throughput());
    // single-layer, single-page fixture; varying feature_count varies page
    // size and the (decode + spatial-query + style-lookup + paint) workload.
    // viewport scales with feature count so all features are inside.
    for &feature_count in &[10u64, 100, 1000] {
        let fixture = rt.block_on(build_fixture_with(FixtureOptions {
            feature_count,
            label_survival: LabelSurvival::Independent,
            ..FixtureOptions::default()
        }));
        let extent = (feature_count as f64) * 10.0 + 10.0;
        let plan = RenderPlan {
            layers: vec![fixture.layer_id.clone()],
            bbox: Bbox::new(0.0, 0.0, extent, extent),
            width: CANVAS_W,
            height: CANVAS_H,
            crs: CrsCode::new(REQUEST_CRS),
            format: ImageFormat::Png,
            scale_pixel_size_m: OGC_STANDARDIZED_PIXEL_SIZE_M,
        };

        let id = BenchmarkId::from_parameter(format!("features_{feature_count}"));
        group.bench_with_input(id, &(fixture.runtime.clone(), plan), |b, (runtime, plan)| {
            b.iter(|| {
                let bytes = rt.block_on(runtime.render(plan)).unwrap();
                black_box(bytes);
            });
        });
    }
    group.finish();
}

fn bench_crs(c: &mut Criterion) {
    let rt = tokio_rt();
    let mut group = c.benchmark_group("runtime_render_e2e_crs");
    group.throughput(pixel_throughput());
    let fixture = rt.block_on(build_fixture_with(FixtureOptions {
        feature_count: 100,
        ..FixtureOptions::default()
    }));
    // page extent for feature_count=100 with the diagonal layout.
    let extent = 100.0 * 10.0 + 10.0;

    // same-crs baseline: request CRS == binding native CRS, no projection.
    let plan_same = RenderPlan {
        layers: vec![fixture.layer_id.clone()],
        bbox: Bbox::new(0.0, 0.0, extent, extent),
        width: CANVAS_W,
        height: CANVAS_H,
        crs: CrsCode::new(REQUEST_CRS),
        format: ImageFormat::Png,
        scale_pixel_size_m: OGC_STANDARDIZED_PIXEL_SIZE_M,
    };

    // cross-crs: request EPSG:3857; binding still EPSG:25832. forces a
    // per-feature transform on the geometry path. bbox is reprojected to
    // 3857 so the spatial query still hits the page.
    let cross_xform = mars_proj::cached_transformer(&CrsCode::new(REQUEST_CRS), &CrsCode::new("EPSG:3857")).unwrap();
    let mut corners = vec![[0.0, 0.0], [extent, 0.0], [extent, extent], [0.0, extent]];
    cross_xform.transform_points(&mut corners).unwrap();
    let (mut min_x, mut min_y, mut max_x, mut max_y) =
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for [x, y] in corners {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let plan_cross = RenderPlan {
        layers: vec![fixture.layer_id.clone()],
        bbox: Bbox::new(min_x, min_y, max_x, max_y),
        width: CANVAS_W,
        height: CANVAS_H,
        crs: CrsCode::new("EPSG:3857"),
        format: ImageFormat::Png,
        scale_pixel_size_m: OGC_STANDARDIZED_PIXEL_SIZE_M,
    };

    for (label, plan) in [("same_crs", plan_same), ("cross_crs", plan_cross)] {
        let id = BenchmarkId::from_parameter(label);
        let runtime = fixture.runtime.clone();
        group.bench_with_input(id, &(runtime, plan), |b, (runtime, plan)| {
            b.iter(|| {
                let bytes = rt.block_on(runtime.render(plan)).unwrap();
                black_box(bytes);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_layer_scaling, bench_feature_density, bench_crs);
criterion_main!(benches);
