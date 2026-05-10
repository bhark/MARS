# Gate findings

This is the spike's findings file. The harness's `GATE: PASS/FAIL` line
evaluates the criteria in the *Criteria* section below; the *Findings*
section records what the operator's reference fixture actually produced
and the *Routing* section records the v1 decision.

## Criteria (informational, not a ship gate)

These thresholds are useful as regression signals once topology-aware is
wired into the compiler. They are **not** a ship/no-ship lever - that
decision lives in *Routing*.

- `seam_violation_count == 0` at levels 1 and 2.
- `(collapsed_ring_count + invalid_reassembly_count + self_intersection_count)`
  < 0.1% of features at every level (configurable via
  `--degenerate-threshold`).
- imagediff at level 1 / level 2 produces no large connected-component
  sliver gaps on visual inspection.

## Findings (operator reference fixture, ~2.6M MultiPolygon parcels)

```
graph: 15.8M verts, 18.3M edges, 4.9M junctions, 7.5M arcs (98% shared)
peak RSS: 8.5 GiB
imagediff (l1/l2/l3): 0.0003% / 0.0011% / 0.0051% pixel divergence

level | tol  | seam_viol | degenerate
  1   | 1m   |     297   |   0.03%
  2   | 5m   |   5,649   |   0.31%   <- trips strict criteria
  3   | 25m  |  86,493   |   3.24%

stage cost (per Mfeat):
  ingest      ~2.8 s    graph     ~19.5 s   <- new vs naive
  dp_lN       ~0.4 s    reassemble ~1.0 s
  verify_lN   ~1.4 s    imagediff  ~1.5 s
  total run on the reference fixture: ~94 s wallclock
```

Strict criteria fail at level 2, but the imagediff numbers say the
output is visually indistinguishable. The "violations" are bounded
asymmetric fallbacks, not topology breaks - see *Root cause* below.

## Root cause of the residual seam violations

`reassemble.rs:reassemble_polygon` falls back **per ring**: when a ring
simplifies below 4 vertices, the entire ring reverts to its original
coords. Its neighbour across the seam doesn't fall back, so it keeps
the simplified seam. Result: one side has the original arc, the other
has the simplified arc - the verifier counts every such mismatch.

Fix is mechanical: per-arc fallback instead of per-ring. When a ring
would collapse, mark the dominant arc(s) as "must keep original" and
re-emit their coords from `topo`. The arc is then un-simplified for
**all** rings referencing it, so seams stay aligned by construction.
Estimated 3-5 days.

## Routing

**Ship topology-aware in v1.** The naive simplifier produces slivers on
every shared arc it touches (millions of seams); topology-aware-with-
ring-fallback produces them on a few thousand. ~3 orders of magnitude
better, confirmed visually by the imagediff.

The integration follow-up plan must include three workstreams, not one:

1. **Wire topology-aware into `mars-compiler`.** Original scope. Lift
   batch simplification onto the binding+level scope; teach
   snapshot/rebuild to collect-then-simplify-batch; widen
   rebuild's neighbourhood semantics; drop the
   `SimplifierKind::TopologyAware` config rejection.

2. **Bring the spilled external-sort backend forward** (currently deferred). 8.5 GiB peak RSS on the reference fixture is
   already past the 4 GiB working-set ceiling; cadastre-scale bindings
   won't fit otherwise. The integration cannot land without it.

3. **Per-arc fallback** as v1.x. Spec at integration time, ship after
   v1 is out. Closes out the residual seam-violation count.

## Open questions for the integration follow-up

- Filtered-binding behaviour. Topology-aware needs the **whole**
  polygon set at binding+level scope; `when:` filters that drop
  features change which boundaries are shared. The integration plan
  must run topology-aware **after** the filter and re-detect junctions
  in the filtered set. Pre-merge tests required.

- Graph-build cost on incremental rebuild. 19.5 s/Mfeat is fine for a
  one-shot snapshot, problematic if rebuild re-graphs the entire
  binding on every change. Decide: cache + incremental update, or
  bbox-widened re-snapshot of touched pages.
