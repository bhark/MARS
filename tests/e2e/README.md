# MARS kind e2e suite

A true end-to-end harness: spins up a [kind](https://kind.sigs.k8s.io/) cluster,
builds the `mars` and `mars-operator` images locally, helm-installs the
operator chart, then asserts production-equivalent behaviour through a real
`MarsService` CR pointing at an in-cluster SeaweedFS (S3-compatible) and PostGIS.

## Prerequisites

- `docker`, `kind`, `helm`, `kubectl`, `cargo`
- A copy of the representative test dataset (gzipped SQL dump). The dataset
  itself is not in git - see "Fixture" below.

## Running

```sh
# full pipeline (build images, kind cluster, helm install, cargo test, teardown)
scripts/run-e2e.sh

# keep the cluster up for inspection on success or failure
MARS_E2E_KEEP=1 scripts/run-e2e.sh

# run a single test
scripts/run-e2e.sh --test bootstrap

# during iteration: skip the docker build of mars images
scripts/run-e2e.sh --skip-image-build
```

Diagnostics on failure land in `target/e2e-output/<run-id>/` (kubectl describe
pods, logs for every container, events ordered by timestamp).

## Layout

```
tests/e2e/
├── Cargo.toml                    # excluded from the root workspace
├── kind.yaml.tmpl                # cluster config template (rendered by run-e2e.sh)
├── src/                          # driver crate (kube::Client wiring, http,
│                                 # deploy helpers, waits)
├── tests/
│   ├── e2e_suite.rs              # single test binary; declares submodules
│   └── e2e_suite/
│       ├── scenario.rs           # shared per-test setup
│       ├── a_bootstrap.rs        # cluster boots, health green, manifest published
│       └── c_rendering.rs        # WMS smoke (200 + PNG magic + body size)
├── manifests/                    # hand-rolled YAML, simple `{{KEY}}` templating
└── scripts/fetch-fixture.sh      # downloads the dataset to target/e2e-fixtures/
```

The crate is excluded from the root `[workspace]` (see `/Cargo.toml`) so
`cargo test --workspace` doesn't pull in kube/k8s-openapi/kind.

## Fixture

The test dataset is a gzipped Postgres dump that creates an `e2e_source`
schema with six layers (land, water, settlements, roads, buildings,
waterways) in EPSG:25832. The contract is documented in
`tests/integration/fixtures/local-map-subset/README.md`; the dump itself is
intentionally **not** committed.

`scripts/fetch-fixture.sh` resolves the dump in this order:

1. `target/e2e-fixtures/local-map-subset.sql.gz` if present and the SHA256
   matches `tests/e2e/fixtures/manifest.sha256`.
2. Otherwise fetch from `${MARS_E2E_FIXTURE_URL}`.
3. Otherwise error with a pointer back to this README.

To skip the fetch and use a local dump:

```sh
MARS_E2E_FIXTURE_PATH=/path/to/dump.sql.gz scripts/run-e2e.sh --no-fetch
```

The kind cluster mounts `target/e2e-fixtures/` into the control-plane node;
the in-namespace fixture-loader Job reads the dump from there.

## Cost

~3-5 minutes per test (PostGIS extension install + fixture load + compiler
first cycle). The suite gates on the `e2e-k8s` PR label and a nightly cron;
not on every PR.

## Scope discipline

The kind suite earns its keep on end-to-end glue (chart → operator → deployed
pods → S3 → "the binary serves a tile"). Component-level concerns belong
elsewhere:

- Pixel-level render correctness → `tests/parity/` (MapServer-anchored).
- S3 publish/read semantics → `mars-store-s3` integration test (with a Garage
  testcontainer) — not in this suite.
- Operator reconcile logic on individual fields → `kube::Client` fake-api
  tests in `bin/mars-operator` — not in this suite.
- Helm chart vs CRD drift → already enforced by `mars-operator print-crd` +
  the chart's `templates/crd.yaml` drift check in CI.
- WMS/WMTS request parsing edge cases → per-crate unit tests.
