#!/usr/bin/env bash
set -euo pipefail

# fetch the public OSM dump (shared with the parity suite) into
# target/e2e-fixtures/. the kind suite mounts that dir into the cluster via
# the extra-mount declared in kind.yaml.tmpl, then derive-e2e.sql
# materialises the e2e_source schema on top of planet_osm_*.
#
# URL precedence:
#   1. MARS_E2E_FIXTURE_URL env override (forks / mirrors / air-gapped dev)
#   2. # source: line in tests/parity/fixtures/osm/manifest.sha256
# sha256 verified against the same manifest. asset is data
# (C) OpenStreetMap contributors, distributed under the ODbL; see the
# Release notes for full attribution.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

# shellcheck source=../../../scripts/lib/fetch-fixture-common.sh
. "${ROOT}/scripts/lib/fetch-fixture-common.sh"

fetch_fixture::ensure \
  "${ROOT}/target/e2e-fixtures/osm-parity.sql.gz" \
  "${ROOT}/tests/parity/fixtures/osm/manifest.sha256" \
  "osm-parity.sql.gz" \
  "MARS_E2E_FIXTURE_URL"
