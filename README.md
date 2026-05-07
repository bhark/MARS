# MARS: Map Artifact Rendering Service

MARS is a fast, cloud-first and Kubernetes-friendly single binary for serving WMS and WMTS. It takes a novel approach to self-organizing caching on several tiers, to make scalable WMS/WMTS cheaper, faster and considerably less painful. 

> [!WARNING]
> This is a pre-alpha WIP. It is NOT suitable for production use. 

## What?

MARS is a materialised WMS/WMTS service over PostGIS. A `compiler` watches the source database and writes versioned spatial artifacts to an object store; one or more stateless `runtime` replicas render directly from those artifacts via a local SSD cache and `mmap`.

The split is the whole idea. Classic dynamic WMS servers query and style the database on every request; MARS does that work once, in the background, and the hot path becomes a memory-mapped read plus a CPU rasterise. One Rust binary, two modes (`runtime`, `compiler`, or `all-in-one`), one logical service per process.

## Why?

MapServer and GeoServer are the workhorses of the WMS world, and they show their age. They were built around the "dynamic WMS" model: every GetMap is a fresh database query, fresh styling, fresh rasterisation. That model has real costs:

- **Database load scales with traffic, not data churn.** A dataset that changes every five minutes is queried thousands of times per minute under modest load. The DB becomes the bottleneck for something that is, fundamentally, read-only between updates.
- **Operational shape is awkward.** A 6,000-line MapServer mapfile mounted into a JVM-or-CGI process, fronted by a tile cache, fronted by a reverse proxy, with Postgres connection pools and JVM tuning to match. Cloud-native it is not.
- **Latency variance is high.** p50 looks fine; p95 and p99 are at the mercy of whatever the DB is doing.
- **Caching is bolted on.** MapCache, GeoWebCache, Varnish - each layer is its own moving part with its own invalidation story.

WMS and WMTS still matter. Government and utility GIS stacks (cadastral, environmental, planning, infrastructure) are built around them, the desktop clients (QGIS, ArcGIS) speak them natively, and they are the lingua franca of inter-agency data sharing. Replacing them with vector tiles is a multi-year client migration that many operators cannot afford. MARS exists to give those operators a modern server for the protocols they already use.

## How?

The hot path never touches the source database. All geometry, attributes, class assignments, label candidates and style references are pre-baked into versioned, content-addressed artifact files in an object store (S3, R2, GCS, MinIO, or a local filesystem). A small JSON manifest binds a service version to its full artifact set and is swapped atomically.

Storage and caching form a clean tier stack:

```
object store (shared, source of truth)
   -> node-local SSD cache (per runtime pod)
      -> OS page cache (mmap)
         -> bounded in-process cache (decoded hot structures only)
```

Artifacts are partitioned by `(source_collection, scale_band, cell)` so a render touches only the cells in the viewport at the right zoom. Geometry is stored once, in the source's native CRS; reprojection happens at render time against an allowlist. Styles and class assignments live separately from geometry, so changing a colour does not multiply storage. Updates flow through PostgreSQL logical decoding (`pgoutput`), translated into a set of dirty cells, batched into an incremental recompile window, and published as a new manifest.

The renderer is CPU-only (`tiny-skia` + `rustybuzz` + `cosmic-text`), the binary is statically linked, and the container image is `FROM scratch`. Observability (Prometheus, structured JSON logs, OpenTelemetry traces) is built in, not bolted on.

## When?

MARS is a good fit when:

- You serve WMS and/or WMTS over PostGIS, and the dataset is large but slow-moving (think hundreds of GB, partial updates every few minutes).
- You have many layers sharing a small set of source tables, and the styling is reasonably stable.
- You want predictable latency, low DB load, and a Kubernetes-shaped deployment.
- You have an existing MapServer mapfile or GeoServer config you want to migrate without rewriting clients.
- You need observability and per-request tracing as a first-class concern, not a plugin.

MARS is **not** the right tool when:

- Your data changes faster than you can recompile (sub-second freshness on every feature). MARS targets minutes, not milliseconds; a classic dynamic WMS or a vector-tile pipeline fits better.
- Your source is not PostGIS. v1 supports PostGIS only; other sources are out of scope.
- You need full OGC conformance, SLD on the wire, WFS/WCS/WPS, or transactional editing. MARS aims at correct rendered output and the common interop surface, not the full OGC catalogue.
- You need GPU rendering or pixel-perfect parity with another renderer's anti-aliasing. MARS is CPU-only and aims at "indistinguishable in normal use" rather than bit-exact.
- You want one server multiplexing many unrelated services. MARS is one process per logical service by design; orchestrate multiple with Helm or an operator.

## Contributing

Contributions are welcome. Some ground rules so the project stays coherent.

### Toolchain

- Rust **1.95.0**, pinned via `rust-toolchain.toml` (edition 2024). Don't bump it in a feature PR.
- `unsafe_code` is `deny` workspace-wide. The only exceptions are the PROJ FFI boundary and the mmap wrapper, both clearly scoped.
- Workspace clippy lints warn on `unwrap_used`, `expect_used`, `panic`, `todo`, `dbg_macro`. CI runs with `-D warnings`, so these become hard errors.

### Local checks before pushing

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --all-targets
./scripts/check-hexagonal-architecture.sh   # needs jq
cargo deny check --all-features
```

The end-to-end suite spins up a PostGIS container via `testcontainers` and needs Docker:

```sh
./scripts/run-e2e.sh
```

CI runs the same gates plus `cargo-deny`. A green local run is not a guarantee of a green CI run, but a red local run is a guaranteed red CI run.

### Architecture rules

The codebase is hexagonal: `domain <- ports <- {adapters, application, interfaces} <- bin`. `scripts/check-hexagonal-architecture.sh` enforces this in CI and rejects:

- `tokio`, `axum`, `sqlx`, `object_store`, `tiny-skia`, `hyper`, `reqwest`, `aws-*` appearing as runtime deps in `domain/*` or `ports/*`.
- Concrete-adapter `use` paths (`mars_store_fs`, `mars_source_postgres`, `mars_render`) outside the adapter crate or `bin/`.
- Adapter-specific method calls leaking into application or interface code - if you need it elsewhere, lift it onto the port.
- `#![allow(unsafe_code)]` outside the two whitelisted files.

If you find yourself fighting these rules, that is the signal to step back: the right move is almost always to lift behaviour onto a port or move the call into a bin crate.

### Code style

- Clean, KISS, production-grade by default. Quick hacks are not accepted; if a hack is the only way forward, the design needs a rethink instead.
- Comments earn their place. Use them for non-obvious *why*, hidden invariants, or short structural markers in long files. Don't restate what well-named code already says.
- Each crate uses its own `thiserror` enum; there is no central error crate. Stub adapters return a typed `NotImplemented { what: "..." }` rather than panicking.
- Respect existing conventions in the crate you are touching.

### Commits and PRs

- Use semantic/conventional commits.
- One logical change per PR. Refactors and behaviour changes go in separate PRs where practical.
- Include tests for new behaviour. The image-diff harness gates renderer changes; unit and property tests gate everything else.
- If a change touches the artifact format, the manifest schema, or a port signature, call it out explicitly in the PR description - those are the seams where compatibility matters.

### Licensing

MARS is dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option. By contributing, you agree to license your contribution under the same terms.


> [!NOTE]
> This project has been a long-standing idea too big to swallow. Recent progress in LLM's has made it more approachable, and it is as such in part a research challenge to see how much LLM's can accomplish. Not a single line of code is human-written. We've had to give the LLM considerable handholding throughout, though, and all architecture, creative decisions and design choices has been strictly defined by an experienced systems architect and software engineer. 
