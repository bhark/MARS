#!/usr/bin/env bash
#
# enforces the architecture dependency rule (ARCHITECTURE.md §3.1):
#
# crates under crates/domain/ and crates/ports/ must not have any normal-kind
# (i.e. non-dev, non-build) dependency on a runtime / I/O / rendering crate.
#
# build-deps and dev-deps are excluded so e.g. mars-artifact's planus build-dep
# and any test-only tokio dev-deps don't trip false positives.

set -euo pipefail

if ! command -v jq >/dev/null 2>&1; then
    echo "check-dependency-rule: jq is required" >&2
    exit 2
fi

cd "$(dirname "$0")/.."

# crates whose names must never appear as a normal-kind dep of a domain/port
BANNED=(
    tokio
    axum
    sqlx
    object_store
    tiny-skia
    hyper
    reqwest
)

# match any aws-* crate (e.g. aws-config, aws-sdk-s3)
BANNED_PREFIXES=( "aws-" )

is_protected_path() {
    case "$1" in
        */crates/domain/*) return 0 ;;
        */crates/ports/*)  return 0 ;;
        *) return 1 ;;
    esac
}

is_banned() {
    local name="$1"
    for b in "${BANNED[@]}"; do
        [[ "$name" == "$b" ]] && return 0
    done
    for p in "${BANNED_PREFIXES[@]}"; do
        [[ "$name" == "$p"* ]] && return 0
    done
    return 1
}

violations=0

# stream every (crate, manifest_path, dep_name, dep_kind) tuple
while IFS=$'\t' read -r crate manifest dep kind; do
    is_protected_path "$manifest" || continue
    # kind: "" = normal, "build", "dev"
    [[ "$kind" == "build" || "$kind" == "dev" ]] && continue
    if is_banned "$dep"; then
        echo "VIOLATION: domain/port crate '$crate' has normal-kind dep on banned crate '$dep'"
        echo "           manifest: $manifest"
        violations=$((violations + 1))
    fi
done < <(
    cargo metadata --format-version=1 --no-deps \
        | jq -r '
            .packages[]
            | . as $p
            | $p.dependencies[]
            | [$p.name, $p.manifest_path, .name, (.kind // "")]
            | @tsv
        '
)

if (( violations > 0 )); then
    echo
    echo "$violations dependency-rule violation(s) found." >&2
    echo "see ARCHITECTURE.md §3.1 (async-boundary rule)." >&2
    exit 1
fi

echo "dependency rule: ok"
