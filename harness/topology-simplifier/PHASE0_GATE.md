# Phase 0 gate

This document defines the gate the LAZARUS Phase 0 spike must clear before
v1 can ship topology-aware simplification, and the report fields the
operator captures for the integration follow-up to use as its starting
data set.

The gate decision itself (pass / fail per fixture run, plus the resulting
v1 routing) is recorded in the project's running notes outside git -
this document defines *the criteria*, not the result.

## Criteria

A fixture run **passes** iff:

1. `seam_violation_count == 0` at levels 1 and 2 (the two finest decimation
   tolerances). Any seam violation at the two finest levels means the
   simplification produced a topology break a renderer would show as a
   sliver gap. Level 3 (the coarsest tolerance) is allowed to violate
   because at that scale geometric collapse is expected; the gate measures
   degradation rate there, not zero.

2. `(collapsed_ring_count + invalid_reassembly_count + self_intersection_count)`
   < `degenerate_threshold * features_in` at every level. The default
   threshold is 0.1% (`--degenerate-threshold 0.001`).

3. The image cross-check produces no large connected-component sliver
   gaps at level-1 / level-2 zoom-equivalent canvases on visual
   inspection. This is **informational** - the verifier already covers
   the topological version of the same property; the image step is for
   eyeballing pathological cases the substring check would not flag
   (e.g. floating-point drift between identical-on-paper sequences).

A fixture run **fails** iff any of the above are violated.

## Report fields the operator captures

Every fixture run emits these to stderr and (where relevant) to the
`--out` directory. Capture the full block in the running-notes entry.

### Per-fixture (one set per run)

- `ingest_*`: line counts and skip reasons.
- `graph_*`: feature_count, ring_count, vertex_count, edge_count,
  junction_count, arc_count, shared_arc_count, island_arc_count.
- `peak RSS` (KiB and MiB) - whole-process high-water mark from
  `/proc/self/status:VmHWM`. **Critical for the integration follow-up's
  scaling question**: the compiler's existing in-memory binding-page
  working set is ~hundreds of MiB; topology-aware adds the graph + arc
  index on top, and that delta determines whether the per-binding
  working-set ceiling holds.

### Per-level (one set per `--tolerance-m` entry)

- `rings`, `collapsed_ring_count`, `collapsed_arc_count`,
  `invalid_reassembly_count` (hole-outside-shell), `self_intersection_count`.
- `shared_arc_count`, `seam_violation_count` (the gate signal).
- `imagediff_*`: differing-pixel count and percentage; PNG paths.

### Timings (one block per run, every stage)

`ingest`, `graph`, `dp_l<n>`, `reassemble_l<n>`, `verify_l<n>`,
`imagediff_l<n>`. Reported as `<wallclock> ms (= <normalised> ms/Mfeat)`.
The graph-build line is the one to watch: it's the stage that does not
exist in the current naive simplifier, so its per-Mfeat number is the
upper bound on the cost the integration follow-up has to absorb.

## Open question: filtered bindings

Topology-aware simplification needs the **whole** polygon set at a given
binding+level scope. The compiler today supports `when:` filters that
prune which features participate in a binding. Two scenarios the spike
does **not** answer because the harness operates on a flat dump:

1. A filter that drops a polygon between two neighbours (e.g. parcels
   filtered by `usage_type`): the survivors no longer cover the same
   plane. Topology-aware on the survivors will emit cleanly noded arcs
   for the survivors, but the rendered image will show the dropped
   polygon's footprint as background, and the survivor's "shared" edge
   (now non-shared in the filtered set) will be an unshared boundary.
   This is correct, just visually different from the unfiltered case.
   Action: the integration follow-up must run topology-aware after the
   filter, not before, and must re-detect junctions in the filtered set.

2. Per-tile filters on top of the binding-level filter (e.g. spatial
   index pruning during runtime): topology-aware is a compile-time
   pass over the binding, so this is not a problem in principle - but
   the integration follow-up needs to confirm that the runtime's
   per-tile feature subset, drawn from a binding compiled with
   topology-aware simplification, still produces clean tile boundaries
   (it should: the per-tile subset's ring vertices are exactly the
   binding-level vertices, so seams remain consistent).

These scenarios cannot be exercised by the harness (no `when:` parser,
no runtime). The integration follow-up plan must include test fixtures
that exercise both - flag them as required pre-merge tests when
opening that plan.

## Routing

- **Pass.** Open the integration follow-up plan with the captured peak
  RSS, per-stage timings (especially graph-build), and the filtered-
  binding open question explicitly carried forward as required follow-up
  work.

- **Fail.** Tick the Phase 0 boxes, record the decision in the running
  notes (LAZARUS bailout 1, line 499), and ship v1 with naive DP plus
  a documented limitation. Topology-aware deferred to post-v1.
