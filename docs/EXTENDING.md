# Extending MARS

How to land a new feature without disturbing the rest of the codebase. The
shape of each extension differs by layer; pick the matching section.

The architecture script (`scripts/check-hexagonal-architecture.sh`) enforces
layering on every push. Stay inside the patterns below and it won't object.

---

## 1. New `DrawOp` variant (e.g. raster, gradient, composite symbol)

Worked example: shipping `DrawOp::Gradient`.

1. **Port.** Add the variant to `crates/ports/mars-render-port/src/lib.rs`,
   doc-commented with what fields it carries. Do **not** add `#[non_exhaustive]`
   to `DrawOp` - the dispatch hub uses exhaustive `match` as the build-time gate.
2. **Dispatch hub.** Add a `match` arm in `crates/adapters/mars-render/src/ops/mod.rs`.
   If the pipeline is not wired yet, return
   `Err(RenderError::NotImplemented { what: "DrawOp::Gradient" })` and stop -
   that is a valid landing state and keeps the contract honest.
3. **Pipeline.** Create `crates/adapters/mars-render/src/gradient/` (mirrors
   `fill/`, `stroke/`, `label/`). The entry function takes the Resolved view,
   not raw Style.
4. **Resolved.** If the variant reads Style fields, fold them into
   `prepare::Resolved` so call sites read post-normalisation only.
5. **Runtime emission.** In `crates/application/mars-runtime/src/render/`, emit
   the new variant when appropriate (often `project.rs`). Do not break existing
   tessellation paths; only emit the new variant for cases that the old path
   couldn't handle.
6. **Tests.** Add a unit test in the new pipeline module asserting a known
   pixel/byte outcome. Add a `NotImplemented` test if you stopped at step 2.

---

## 2. New `FillPaint` / `MarkerSymbol` / Style field

1. **Vocabulary.** Add the variant or field in `crates/domain/mars-style/src/lib.rs`,
   serde-tagged. Mark enums `#[non_exhaustive]`.
2. **Resolved.** Extend `prepare::resolve` in `mars-render/src/prepare.rs`. If
   the field can be ignored without breaking output, route it through
   `UnimplementedFeatures` so the dispatch hub warns instead of silently
   dropping.
3. **Pipeline module.** Add a sibling under `fill/`, `stroke/`, or `label/`,
   then dispatch from the parent `mod.rs` via exhaustive match.
4. **Tests.** Locality: each pipeline module owns its tests. No central test
   crate for renderer behaviour.

---

## 3. New WMS or WMTS operation (e.g. `GetLegendGraphic`)

1. **Parse.** Extend `crates/interfaces/mars-wms/src/parse.rs` (or `wmts/`) to
   recognise the operation name and required params. Return a typed request
   struct.
2. **Handler module.** Create `crates/interfaces/mars-wms/src/legend.rs` for
   the operation. One file per operation; mirror `feature_info.rs`.
3. **Dispatch.** Wire the handler in `lib.rs` so the HTTP layer routes to it.
4. **Capabilities.** Update `capabilities.rs` so the operation is advertised.
5. **Tests.** Unit tests in the handler module; integration tests in
   `crates/application/mars-runtime/tests/` if the handler crosses into
   runtime.

---

## 4. New source backend (e.g. WFS-cascade, GeoPackage, MVT)

1. **Read the port.** `crates/ports/mars-source/src/access.rs` defines what a
   source must do. If your backend needs something the port doesn't model,
   widen the port **first** in its own commit. Do not add adapter-specific
   methods.
2. **New crate.** `crates/adapters/mars-source-<backend>/`. Pin deps in the
   root `Cargo.toml` `[workspace.dependencies]`, reference with
   `workspace = true`.
3. **Compose.** Wire the adapter in `bin/mars-bin-shared/src/lib.rs` behind a
   config discriminator (`source:` key in `mars.yaml`).
4. **Tests.** Per-adapter integration tests in the adapter crate. Gate
   testcontainers-style tests behind a `MARS_<BACKEND>_IT=1` env flag,
   matching the postgres pattern in `crates/adapters/mars-source-postgres/`.

---

## 5. New style dialect importer (e.g. SLD/SE, QGIS, CSS)

Mirror `bin/mars-import-mapfile/`. It is the canonical worked example.

1. **New bin crate.** `bin/mars-import-<dialect>/`. Pipeline shape:
   `scanner -> parser -> translate -> emitter`.
2. **Output `mars-style`.** The translator's only job is to map the dialect
   onto the canonical `mars-style` types. If the dialect carries something
   `mars-style` cannot express, extend `mars-style` in its own commit first
   (see section 2), then come back to translation.
3. **Round-trip tests.** For each translated construct, assert the emitted
   `mars-style` JSON/YAML matches a fixture. These tests are how you
   demonstrate parity with MapServer/GeoServer one construct at a time.
4. **Do not bypass `mars-style`.** No direct path from dialect AST to
   `DrawOp`. The canonical vocabulary is the seam; that is what makes the
   pattern repeatable.

---

## Cross-cutting rules

- One scope per commit. Subject-only conventional commits (no body, no
  trailers). See `CLAUDE.md` for scope names.
- Never add `#![allow(unsafe_code)]` outside the two whitelisted files
  (`mars-proj`, `mars-store-fs`).
- Don't reach for concrete adapter types in `domain/*`, `ports/*`,
  `application/*`, or `interfaces/*`. The architecture script will reject it.
- New deps: pin in root `Cargo.toml` `[workspace.dependencies]`,
  reference with `workspace = true`.
- `NotImplemented` is a valid landing state. Prefer a typed
  `Err(RenderError::NotImplemented { what })` (or the equivalent in the
  layer's error enum) over a `tracing::debug` that silently drops the
  feature.
