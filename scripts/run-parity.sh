#!/usr/bin/env bash
# Thin driver for the parity suite (tests/parity, workspace-external). Requires
# a docker daemon and the OSM extract dump at target/parity-fixtures/.
# See tests/parity/fixtures/osm/README.md for how to produce the dump.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo test --manifest-path tests/parity/Cargo.toml --release "$@"
