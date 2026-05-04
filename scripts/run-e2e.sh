#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export MARS_E2E=1
exec cargo test -p mars --features e2e -- --nocapture "$@"
