# Contributing to MARS

Thank you for considering a contribution. MARS is early WIP; the public surface, internal modules, and on-disk artifact format are all still in motion.

## Toolchain

- Rust **1.95.0**, pinned via `rust-toolchain.toml`. Edition 2024.
- The workspace denies `unsafe_code` everywhere except the `mars-proj` PROJ FFI boundary and the `mars-store-fs` mmap wrapper.
- `cargo fmt`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked --all-targets` must all pass.

## Architecture

MARS is a hexagonal codebase (domain / ports / adapters / application / interfaces / bin). The dependency direction is enforced in CI by `scripts/check-hexagonal-architecture.sh`. Read it before bending the rules.

If you find yourself wanting to import a concrete adapter from outside its crate, that is the signal to lift the behaviour onto a port instead.

## Commit style

Commits are **subject-only conventional commits**. No body, no co-author trailer.

```
<type>(<scope>): <short imperative>
```

- Types: `feat`, `fix`, `style`, `test`, `docs`, `chore`, `refactor`, `perf`, `ci`.
- Scope mirrors the touched crate (`runtime`, `compiler`, `wms`, `wmts`, `render`, `proj`, `grid`, `store-s3`, `store-fs`, `source-postgres`, `artifact`, `expr`, `style`, `types`, `config`, `observability`, `http`, `text`, `bin-shared`, `diff-capture`, `mars`, ...). One scope per commit.
- Commit each individual piece of work as its own commit once tests + clippy are green. Do not batch commits at the end of a multi-step task.

Releases are automated from these commit messages by `release-plz`; non-conforming commits are skipped from the changelog.

## Running tests

```sh
# unit + integration
cargo test --workspace --locked --all-targets

# in-process integration (testcontainers; requires docker)
MARS_INTEGRATION=1 cargo test -p mars --features integration -- --nocapture

# docker compose integration (postgis + compiler + runtime against a fixture dump)
./scripts/run-integration.sh
```

CI runs the quick tier (`fmt`, `clippy`, `test`, `cargo-deny`, hex-arch) on every push and PR. The integration tier runs on PRs and on release tags. The kind-based e2e suite lives under `tests/e2e/` and is gated separately.

## Releases

Releases are PR-driven via `release-plz`. Conventional commits land on `main`, `release-plz` opens a release PR that bumps `[workspace.package].version` and updates `CHANGELOG.md`. Merging the PR tags `vX.Y.Z`, which triggers binary + container publishing.

## License

By contributing, you agree that your contributions will be dual-licensed under MIT and Apache-2.0, matching the rest of the workspace.
