# Extending MARS

The goal of this document is to make MARS feel like a codebase that grows by
addition. We want OGC parity reached one MapServer/GeoServer feature at a
time, each feature a small PR, each PR limited to a few known files. New work
should plug into existing seams rather than reshape them.

This is the foundational reference for that work. It states the principles we
use, points to where they already hold (mostly the render adapter, which has
been refactored to make them visible), and names the growth areas where they
should be extended. When in doubt about a structural decision, return here.

---

## The principles

Six principles, in priority order. They reinforce each other; each one alone
helps, all six together is what makes the addition feel cheap.

### 1. A canonical vocabulary sits at every seam

Between two layers, name one set of types and require both sides to speak only
in that set. The seam is the *vocabulary*, not the wire format and not the
implementation.

- Renderer seam: `mars-render-port`'s `DrawOp`, `Path`, `Style`. The runtime
  emits these; the adapter consumes these. Neither side names tiny-skia.
- Style seam: `mars-style`'s `Style`, `FillPaint`, `MarkerSymbol`. Importers
  (mapfile today, SLD tomorrow) translate *into* these. The renderer reads
  *from* these. No dialect ever reaches a draw call directly.
- Source seam: `mars-source`'s `access` trait set. Postgres, future WFS or
  GeoPackage, all enter the application through the same vocabulary.

If you find yourself defining a type "for the X adapter" outside the X crate,
you are eroding a seam. Stop and lift the type, or admit it shouldn't cross.

### 2. Exhaustive dispatch is the build-time gate

Where a vocabulary is an enum, the dispatch site is a `match` with no `_`
arm. Adding a variant breaks the build at one known location, which forces
the conversation about whether the new feature is wired or staged.

- `crates/adapters/mars-render/src/ops/mod.rs::dispatch` is the canonical
  example. Adding a `DrawOp` variant breaks here, and only here.
- The same shape applies to `FillPaint`, `MarkerSymbol`, future WMS
  operations, future source kinds.

Mark domain enums `#[non_exhaustive]` for *external* serde compatibility, but
do **not** mark dispatch enums `#[non_exhaustive]` and do **not** add `_`
arms in your own dispatch. The build-time gate is the whole point.

### 3. Normalise once at the boundary

Layers above the boundary speak `Option`-heavy, validation-loose vocabulary.
The first thing the layer below does is collapse that into a non-`Option`,
validated form. Every downstream call site reads from the normalised form
only.

- `crates/adapters/mars-render/src/prepare.rs::resolve` is the worked
  example: Style with ten optional fields becomes `Resolved` with concrete
  alpha, width, cap, join, dash, offset. Stroke and fill pipelines never
  touch a raw `Option` again.
- `crates/interfaces/mars-wms/src/prepare/` and
  `crates/interfaces/mars-wmts/src/prepare/` apply the same shape on the
  request side: per-operation `Parsed*` -> `Resolved*` (`ResolvedGetMap`,
  `ResolvedGetFeatureInfo`, `ResolvedGetLegend`, `ResolvedGetTile`) with a
  shared `ResolvedViewport` chokepoint so allowlist + bound + axis-order
  checks live in one place and KVP / REST cache keys cannot drift.
- A new optional field lands in `resolve` once, not in every consumer.
- The same shape belongs in compiler input handling and in source query
  planning when they grow defaults.

If two call sites are both unwrapping the same `Option` with the same default,
your normalisation layer is missing a field.

### 4. Cohesion follows variant boundaries

When a vocabulary type has variants, give each variant its own module under a
sibling directory, and let a parent `mod.rs` dispatch. New variants land as
new files plus one new match arm.

- `mars-render/src/fill/{solid, hatch}.rs`, `stroke/{base, dash}.rs`,
  `label/{halo, compose}.rs`, `encode/{png, jpeg}.rs`. Each is one variant,
  one file, one match arm.
- Apply the same shape to any pipeline with kinds: source backends,
  importers, WMS operations, future raster pipelines.

This is the principle that keeps files small without losing locality. The
test for a variant lives next to its implementation, not in a central test
crate.

### 5. Typed `NotImplemented` is a valid landing state

A feature can land its vocabulary before its implementation, and the type
system carries the truth. Callers get a typed `NotImplemented`, not a silent
log line and a half-rendered output.

- `RenderError::NotImplemented { what }` and the `Resolved::unimplemented`
  flag set are the two patterns. The first is for variants the adapter
  cannot handle at all; the second is for fields it can ignore but should
  warn about.
- This is what makes one-PR-per-feature workable. The vocabulary commit can
  land months before the implementation commit, and runtime behaviour stays
  honest in between.

A `tracing::debug!("not yet implemented")` that lets the call return
"successfully" is the anti-pattern. If you see one, replace it with a
typed signal.

### 6. Ports name only domain types

Port traits live in `crates/ports/*` and import only domain crates and
`std`. They never name an adapter type and never name a backend library
(`tiny-skia`, `sqlx`, `aws-*`, `tokio` types beyond the bare minimum).

- `mars-render-port` mentions `mars-style::Style` and `mars-types::ImageFormat`,
  not `tiny_skia::Pixmap`.
- `mars-store` mentions paths and bytes, not `aws-sdk-s3` types.
- Adapter-internal normalisation (`Resolved` in `mars-render`) is
  `pub(crate)` and never exported.

If you need a type that is "almost-a-port-type but with one extra field for
my adapter's convenience," that field belongs in the adapter, not in the
port.

---

## Worked example: the render adapter

The render adapter is the cleanest application of all six principles today,
and a useful reference to compare other areas against.

```
crates/ports/mars-render-port/src/lib.rs        principle 1, 6: vocabulary, port
crates/adapters/mars-render/src/
  lib.rs                                        wiring + Renderer impl
  ops/mod.rs                                    principle 2: exhaustive dispatch
  prepare.rs                                    principle 3: normalisation + 5: NotImpl flags
  fill/{solid, hatch}.rs                        principle 4: variant per file
  stroke/{mod, dash}.rs                         principle 4 (base in mod.rs, dash helper sibling)
  label/{halo, compose}.rs                      principle 4
  encode/{png, jpeg}.rs                         principle 4
```

To add a feature here, you touch the canonical vocabulary in `mars-render-port`
or `mars-style`, extend `prepare::resolve` if it adds a field, add a sibling
module under the right directory, add the dispatch arm. That is the entire
shape. If a feature would require touching anything outside this list, the
principles are telling you that the abstraction is wrong, not that the
feature is hard.

---

## Where to apply this next

Areas of likely growth, ranked by ROI for OGC parity. For each, the principle
that applies most directly.

- **WMS / WMTS operations** (`crates/interfaces/mars-wms`, `mars-wmts`).
  Per-operation parse split is done (`parse/{mod,get_map,get_feature_info,
  get_legend,get_tile,common}.rs`), and the request-side `prepare`
  normalisation (principle 3) has landed: `parse/*` returns Option-heavy
  `Parsed*`, `prepare/*` produces `Resolved*` with every default and
  validation applied exactly once. Shared viewport checks live in
  `prepare/viewport.rs`; WMTS REST and KVP both flow through
  `prepare::resolve_get_tile` so cache keys cannot drift. Next operations
  land one module per request - `DescribeLayer`, WMS dimensions, vendor
  params - as a new `parse_*` + `prepare/*_*.rs` pair plus one new
  `WmsRequest` / `WmtsRequest` variant. Response formatting stays at the
  crate root (`capabilities.rs`, `feature_info.rs`, `exception.rs`).

- **Style and filter dialects** (`bin/mars-import-mapfile/`). The shape is
  scanner -> parser -> translate -> emit, with per-block parse/emit modules
  under `translate/` and a `translate/resolved.rs` collapsing Option-heavy
  parsed forms into validated `Resolved*` (mirroring `mars-render`'s
  `prepare.rs`). The seam (principle 1) is the canonical `mars-style`
  vocabulary; never bypass it. SLD/SE, QGIS, CSS-style - each is a parallel
  bin crate that ends at `mars-style`.

- **Source backends** (`crates/ports/mars-source`, `adapters/mars-source-*`).
  Today's port is shaped for postgres. Before the second backend lands,
  re-read principles 1 and 6: does the port speak in source-agnostic
  vocabulary, or has postgres' shape leaked in? Widen the port in its own
  commit if needed; never let an adapter add methods downstream code calls.

- **Compiler stages** (`crates/application/mars-compiler`). The three
  pipelines (snapshot, cycle, rebalance) live as flat orchestrators under
  `stages/` with one module per stage and an explicit per-pipeline ctx
  (`SnapshotCtx`, `CycleCtx`, `RebalanceCtx`) carrying typed inputs +
  outputs between steps; cross-pipeline helpers (`merge`, `noop_bump`,
  `governors`, `sidecars`) live under `stages/shared/`. Principle 4 in
  sequence form: each `stages/<pipeline>.rs` reads as a ~6-line linear
  call list, each `stages/<pipeline>/*.rs` is one stage testable in
  isolation. New compile stages (attribute encoding, label-candidate
  generation, class assignment) land as a new module under the right
  pipeline directory plus one new call in the orchestrator. Pipeline-
  scoped state (`MemoryGovernor`, `DiskGovernor`) lives on the ctx;
  leader-lock-scoped state (`cycle_counter`) stays on `Compiler`. Watch
  for hidden coupling that returns via local-variable mutation in the
  orchestrators - that is principle 3 missing.

- **Symbol and raster rendering.** Symbol dispatch lives under
  `mars-render/src/symbol/`: `mod.rs` is the exhaustive `MarkerSymbol`
  match (rotates + translates the local-frame polygon, delegates to
  `ops::path::draw` for fill+stroke), and the seven geometric variants
  (`circle, square, triangle, cross, x, pin, vector_shape.rs`) each
  carry their own `build_path`. `Glyph` continues to surface through
  `UnimplementedFeatures::glyph_marker`; implementing it is a one-commit
  follow-up that routes through `label/compose::stamp_axis` /
  `stamp_rotated` with a single-char text run. Pattern fills follow the
  same shape under `pattern/`: `DrawOp::Pattern` routes through
  `ops::pattern::draw` and `pattern::draw` dispatches on the
  `FillPaint` variant (today: `Image { name }`, sampling stubbed via
  typed `NotImplemented`; routing-contract errors for procedural
  variants). New non-procedural fills land as a new
  `FillPaint::<Variant>` in `mars-style` + a sibling
  `pattern/<variant>.rs` + one dispatch arm. Raster rendering is a
  larger arc - zero scaffold today, no DrawOp variant, no source
  adapter, no port surface, no importer translation - and may need a
  dedicated adapter (principle 6: keep PROJ-style FFI in support, port
  stays clean) when it lands.

---

## Smells to watch for

When reviewing a PR or a corner of the codebase, these are the signals that
one of the principles is being eroded.

- A `_` catch-all in a dispatch `match`, or a default branch that swallows
  unhandled variants. (Principle 2.)
- Two call sites unwrapping the same `Option` with the same default.
  (Principle 3.)
- A `tracing::debug!` or `tracing::warn!` followed by `Ok(())` for a
  feature that the type system says should have done something. (Principle 5.)
- A type imported from an adapter crate (`mars_render`, `mars_store_fs`,
  `mars_source_postgres`) into a port, application, or interface crate.
  The architecture script catches the obvious cases; review catches the
  subtle ones. (Principle 6.)
- A file in a domain or port crate importing `tokio`, `sqlx`,
  `tiny-skia`, `aws-*`, or other backend libraries. (Principle 6.)
- A new feature whose PR diff spans more than one bounded growth area.
  Either the feature is too big, or one area's seam is in the wrong place.
- An adapter that grew a configuration knob the port doesn't expose.
  Either lift it onto the port (if other adapters need it) or it's
  adapter-internal (and not part of the contract).

---

## Operational hygiene

These rules support the principles. They are documented in `CLAUDE.md`;
repeated here only because they have direct architectural consequences.

- **One scope per commit, subject-only conventional format.** A commit
  that touches `render` and `runtime` is hiding a seam violation, or
  should be two commits.
- **Pin third-party deps once** in the root `Cargo.toml`
  `[workspace.dependencies]`; reference with `workspace = true`. Version
  drift across crates is a structural smell, not a packaging detail.
- **No `#![allow(unsafe_code)]`** outside `mars-proj` and `mars-store-fs`.
  If you think you need it, you almost certainly need to push the FFI down
  into a support crate instead.
- **`scripts/check-hexagonal-architecture.sh` is the gate.** If it fails,
  fix the layering, do not weaken the script.

---

## When the principles don't fit

The principles are descriptive of what works in MARS today, not divine law.
If a real feature genuinely cannot fit, that is a signal worth taking
seriously: usually the right move is to widen a port or split a vocabulary,
not to bypass the seam. Discuss before bypassing. A bypass that ships once
becomes a pattern others copy.
