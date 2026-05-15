#!/usr/bin/env bash
set -euo pipefail

# fetch the public e2e test dataset into target/e2e-fixtures/. the kind suite
# mounts this dir into the cluster via the extra-mount declared in
# kind.yaml.tmpl.
#
# URL precedence:
#   1. MARS_E2E_FIXTURE_URL env override (forks / mirrors / air-gapped dev)
#   2. # source: line in tests/integration/fixtures/local-map-subset/manifest.sha256
# sha256 verified against the same manifest. see scripts/release-fixtures.sh
# for how the manifest is produced.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

# shellcheck source=../../../scripts/lib/fetch-fixture-common.sh
. "${ROOT}/scripts/lib/fetch-fixture-common.sh"

fetch_fixture::ensure \
  "${ROOT}/target/e2e-fixtures/local-map-subset.sql.gz" \
  "${ROOT}/tests/integration/fixtures/local-map-subset/manifest.sha256" \
  "local-map-subset.sql.gz" \
  "MARS_E2E_FIXTURE_URL"
