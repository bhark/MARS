#!/usr/bin/env bash
#
# mirrors the two CI gates that fail whenever workspace deps drift:
#   1. parity lockfile must satisfy --locked against current workspace state
#   2. cargo-deny must pass on advisories, bans, licenses, sources
#
# run before declaring any Cargo.toml/Cargo.lock-touching task done.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "check-deps: cargo-deny is required (cargo install cargo-deny)" >&2
    exit 2
fi

failed=0

echo ">> parity gate: cargo check --locked --manifest-path tests/parity/Cargo.toml --tests"
if ! cargo check --locked --manifest-path tests/parity/Cargo.toml --tests; then
    echo "check-deps: parity lockfile is out of sync; refresh with:" >&2
    echo "    cargo check --manifest-path tests/parity/Cargo.toml --tests" >&2
    failed=1
fi

echo
echo ">> deps gate: cargo deny --all-features check"
if ! cargo deny --all-features check; then
    echo "check-deps: cargo-deny rejected the lockfile (see output above)" >&2
    failed=1
fi

exit "$failed"
