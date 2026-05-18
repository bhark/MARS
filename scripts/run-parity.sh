#!/usr/bin/env bash
set -euo pipefail

# MARS parity harness driver. ensures the OSM dump is present (auto-fetched
# on first run from the GitHub Release pinned in
# tests/parity/fixtures/osm/manifest.sha256), then runs the parity matrix
# under tests/parity/tests/osm.rs against a postgis testcontainer.
#
# usage: scripts/run-parity.sh [--no-fetch] [-- <cargo test args>]
#
# env knobs:
#   MARS_PARITY_FIXTURE_URL=URL   override the manifest's source url
#   MARS_PARITY_FIXTURE_PATH=P    skip fetch + use existing dump at P

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

NO_FETCH=0
CARGO_ARGS=()

usage() {
  cat <<EOF
usage: scripts/run-parity.sh [--no-fetch] [-- <cargo test args>]

env knobs:
  MARS_PARITY_FIXTURE_URL=URL   override the manifest's source url
  MARS_PARITY_FIXTURE_PATH=P    skip fetch + use existing dump at P
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-fetch) NO_FETCH=1; shift ;;
    -h|--help) usage; exit 0 ;;
    # keep the `--` so cargo test sees it as the test-binary arg separator.
    --) CARGO_ARGS+=("$@"); break ;;
    *) CARGO_ARGS+=("$1"); shift ;;
  esac
done

# MARS_PARITY_FIXTURE_PATH lets devs point at a local dump without copying.
# stage it into the expected target path so the test (which mounts a fixed
# path) sees it. an existing file at the target wins to avoid clobbering.
if [[ -n "${MARS_PARITY_FIXTURE_PATH:-}" ]]; then
  dest="${ROOT}/target/parity-fixtures/osm-parity.sql.gz"
  mkdir -p "$(dirname "${dest}")"
  if [[ ! -e "${dest}" ]]; then
    ln -s "${MARS_PARITY_FIXTURE_PATH}" "${dest}"
  fi
  NO_FETCH=1
fi

if [[ "${NO_FETCH}" != "1" ]]; then
  "${ROOT}/tests/parity/scripts/fetch-fixture.sh"
fi

cargo test --manifest-path "${ROOT}/tests/parity/Cargo.toml" --release "${CARGO_ARGS[@]}"
