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

## Running tests

```sh
# unit + per-crate integration that runs without docker
cargo test --workspace --locked --all-targets

# in-process integration (testcontainers; requires docker)
cargo test -p mars --features integration -- --nocapture

# render parity against external reference goldens (workspace-external)
./scripts/run-parity.sh
```

CI runs the quick tier (`fmt`, `clippy`, `test`, `cargo-deny`, hex-arch) on every push and PR. The integration tier runs on PRs and on release tags. The parity suite (`tests/parity/`) and the kind-based e2e suite (`tests/e2e/`) are gated separately.

## Releases

Releases are tag-driven. The tag IS the version: `[workspace.package].version` and `Chart.yaml`'s `version`/`appVersion` stay at the `0.0.0-dev` placeholder on `main` and are rewritten by CI in the runner from the tag at build time. Never bump them in a PR.

Cut a release with the helper script:

```sh
scripts/release.sh patch          # 0.1.4 -> 0.1.5
scripts/release.sh minor          # 0.1.4 -> 0.2.0
scripts/release.sh major          # 0.1.4 -> 1.0.0
scripts/release.sh v0.2.0-rc.1    # explicit (use for prereleases)
```

The script preflights a clean tree on `main`, up-to-date with `origin/main`, with the latest CI run on `main` green. It then tags and pushes. The tag push triggers `.github/workflows/release.yml`, which:

1. Builds release binaries for `x86_64-unknown-linux-gnu`, packages a tarball + sha256, and attaches them to the GitHub Release.
2. Builds + pushes multi-arch (`linux/amd64`, `linux/arm64`) container images to `ghcr.io/bhark/mars` and `ghcr.io/bhark/mars-operator`, tagged with the version, `<major>.<minor>`, `<major>`, and `latest` (the `latest` tag is suppressed for prereleases).
3. Packages the operator chart and pushes it to `oci://ghcr.io/bhark/charts/mars-operator`. The chart `.tgz` and the raw CRD YAML are attached to the GitHub Release for kustomize / GitOps consumers.

Helm OCI has no `:latest` concept; pin a chart version on `helm install`.

`CHANGELOG.md` is hand-maintained in the PR(s) that land work, not at release time.

## License

By contributing, you agree that your contributions will be dual-licensed under MIT and Apache-2.0, matching the rest of the workspace.
