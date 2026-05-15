#!/usr/bin/env bash
set -euo pipefail

# fetch the OSM parity dump into target/parity-fixtures/. consumed by
# tests/parity/tests/osm.rs (mounted into a postgis testcontainer).
#
# URL precedence:
#   1. MARS_PARITY_FIXTURE_URL env override (forks / mirrors / air-gapped dev)
#   2. # source: line in tests/parity/fixtures/osm/manifest.sha256
# sha256 verified against the same manifest. see scripts/release-fixtures.sh
# for how the manifest is produced. the asset contains data
# (C) OpenStreetMap contributors, distributed under the ODbL; see the Release
# notes for full attribution.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

# shellcheck source=../../../scripts/lib/fetch-fixture-common.sh
. "${ROOT}/scripts/lib/fetch-fixture-common.sh"

fetch_fixture::ensure \
  "${ROOT}/target/parity-fixtures/osm-parity.sql.gz" \
  "${ROOT}/tests/parity/fixtures/osm/manifest.sha256" \
  "osm-parity.sql.gz" \
  "MARS_PARITY_FIXTURE_URL"
