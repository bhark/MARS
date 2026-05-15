# Benchmarks

Criterion benches across the workspace, sized to give signal at every
layer of the render and compile pipelines. CI runs them nightly via
`.github/workflows/bench.yml` and uploads the full `target/criterion/`
tree as a workflow artifact (90 day retention). Baselines are not
checked in (machine-dependent noise).

## Running

```sh
# whole suite
cargo bench --workspace \
  --features mars-runtime/test-fixtures,mars-runtime/bench-internals

# one crate
cargo bench -p mars-runtime --features mars-runtime/bench-internals

# one bench
cargo bench -p mars-runtime --bench render_e2e

# faster iteration during local optimization work
cargo bench -p mars-runtime --bench render_e2e -- --quick
```

## Saving and comparing baselines

Before starting a perf optimization pass, snapshot a baseline:

```sh
cargo bench --workspace \
  --features mars-runtime/test-fixtures,mars-runtime/bench-internals \
  -- --save-baseline pre-optimization
```

Subsequent runs auto-compare against the most recent run. To compare
explicitly against a named baseline:

```sh
cargo bench -p mars-runtime --bench render_e2e -- --baseline pre-optimization
```

## Bench inventory

### Composite / end-to-end

| Bench                                    | Group                                | Measures                                                       |
|------------------------------------------|--------------------------------------|----------------------------------------------------------------|
| `mars-runtime/benches/render_e2e.rs`     | `runtime_render_e2e_layer_scaling`   | `Runtime::render` cost as layer count grows (orchestration).   |
|                                          | `runtime_render_e2e_feature_density` | Per-page decode + style + paint cost as feature count grows.   |
|                                          | `runtime_render_e2e_crs`             | Same-CRS vs cross-CRS request (projection overhead).           |

### Hot subsystems

| Bench                                       | Group                                                                     | Measures                                                          |
|---------------------------------------------|---------------------------------------------------------------------------|-------------------------------------------------------------------|
| `mars-runtime/benches/feature_prep.rs`      | `runtime_feature_prep_same_crs` / `_cross_crs`                            | Per-page decode + spatial query + style join + paint.             |
| `mars-runtime/benches/label_collision.rs`   | `runtime_label_collision_count` / `_density` / `_placement` / `_force`    | Greedy collision pass under varying load shapes.                  |
| `mars-runtime/benches/manifest_swap.rs`     | `runtime_manifest_state_build` / `_swap_state`                            | `RuntimeState::from_config_and_manifest` and `swap_state` costs.  |
| `mars-store-fs/benches/cache.rs`            | `store_fs_cache_cold_miss` / `_warm_hit` / `_eviction_pressure` / `_mixed_80_20` | `FsCache::get_or_fetch` under four steady-state shapes.       |

### Compiler

| Bench                                          | Group                          | Measures                                                                  |
|------------------------------------------------|--------------------------------|---------------------------------------------------------------------------|
| `mars-compiler/benches/page_rebuild.rs`        | `page_rebuild`                 | Single-page incremental rebuild (`rebuild_pages`).                        |
|                                                | `compiler_multi_page_rebuild`  | Bulk turnover: 10% / 50% page dirty fraction in one cycle.                |
|                                                | `compiler_full_bootstrap`      | End-to-end `run_snapshot_from_plan` from empty store.                     |
| `mars-compiler/benches/decimation.rs`          | `decimation`                   | RDP simplification at three tolerance levels.                             |

### Domain micro-benches

| Bench                                            | Notes                                                            |
|--------------------------------------------------|------------------------------------------------------------------|
| `mars-artifact/benches/iter.rs`                  | Geometry iteration / decode throughput.                          |
| `mars-artifact/benches/spatial_index.rs`         | R-tree build, query at varying selectivity, combined gate.       |
| `mars-expr/benches/parse_eval.rs`                | Expression parse + eval over 10k rows.                           |
| `mars-render/benches/draw_encode.rs`             | Tiny-skia rasterise + PNG/JPEG encode.                           |
| `mars-render/benches/hatch.rs`                   | Hatched fill regression gate vs solid baseline.                  |
| `mars-proj/benches/transform.rs`                 | PROJ transformer construction + warm transform paths.            |

## Informational gate values

These are approximate `--release`/`--quick` numbers from a developer
laptop, kept for orientation rather than as hard CI gates. Compare
against your local baseline, not these.

| Bench / scenario                                                 | Expected |
|------------------------------------------------------------------|----------|
| `runtime_feature_prep_same_crs/n_40000_sel_500`                  | ≤ 2 ms   |
| `runtime_render_e2e_feature_density/features_100`                | ~100 µs  |
| `runtime_label_collision_count/n_500`                            | ~1 ms    |
| `store_fs_cache_warm_hit/size_1MiB`                              | ~250 µs  |
| `runtime_manifest_swap_state/*`                                  | ~150 ns  |

## Feature flags used by benches

`mars-runtime` exposes two non-default features that bench targets need:

- `test-fixtures` — exposes `mars_runtime::test_fixtures` (in-memory
  Runtime + Manifest + page builders). Auto-enabled for tests and
  benches via the self dev-dep on `mars-runtime` in
  `crates/application/mars-runtime/Cargo.toml`.
- `bench-internals` — exposes `mars_runtime::bench_internals`
  re-exporting `pub(super)` collision API (`PreparedLabel`,
  `PreparedPlacement`, `collide_and_emit_labels`). Required by the
  `label_collision` bench; opted in via `required-features` on that
  bench target. Never enabled in production builds.

## Adding a new bench

1. Drop a `*.rs` file under the right crate's `benches/` directory.
2. Declare it in that crate's `Cargo.toml`:
   ```toml
   [[bench]]
   name    = "my_bench"
   harness = false
   ```
3. Use Criterion's `BenchmarkId` + `Throughput` so the parameter sweep
   reads cleanly in the output.
4. Update this file's bench inventory.
