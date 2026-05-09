# topology-simplifier (Phase 0 spike)

Operator-driven harness for evaluating mapshaper-style topology-aware
polygon simplification on a local polygon fixture (e.g. a cadastre dump).
Workspace-excluded; not built by the main `cargo build --workspace`.

The spike's outcome is a gate decision: does topology-aware simplification
produce visually clean output at coarse decimation levels (level-2+)
without the seam gaps that naive Douglas-Peucker introduces on shared
boundaries? The result is recorded in the project's running notes and
drives whether v1 ships with naive DP plus a documented limitation, or
whether a follow-up integration plan is opened to wire topology-aware
into the compiler.

## Build / run

```sh
cargo build --release --manifest-path harness/topology-simplifier/Cargo.toml

cargo run --release --manifest-path harness/topology-simplifier/Cargo.toml -- \
    --fixture /path/to/polygon-dump.tsv \
    --out /tmp/phase0
```

## Fixture format

Tab-separated values, one feature per line:

```
<feature_id>\t<hex_ewkb>
```

Operator dumps via psql:

```
\COPY (SELECT id, encode(ST_AsEWKB(geom), 'hex') FROM <table>) TO '/tmp/dump.tsv'
```

Anything other than Polygon / MultiPolygon is logged and skipped.

## Output

- per-stage timings (ingest, quantise, graph, junction split, canonicalise,
  DP, reassemble, validity, verify) normalised per million features
- peak RSS observed
- counter values: collapsed_ring_count, collapsed_arc_count,
  invalid_reassembly_count, self_intersection_count, seam_violation_count
- informational image cross-check PNGs at level-1/2/3 tolerances

The gate passes if `seam_violation_count == 0` at levels 1 and 2 and
the degenerate-reassembly counters stay below the configured threshold
(default < 0.1% of features). See `PHASE0_GATE.md` for the full
criteria, the fields the operator captures into the running-notes
entry, and the open questions the integration follow-up must address
(notably filtered-binding semantics).
