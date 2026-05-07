#!/usr/bin/env bash
#
# enforces hexagonal architecture rules (ARCHITECTURE.md §3).
#
# checks:
#   1. domain/port crates must not have normal-kind deps on runtime / I/O crates
#   2. layer dependency direction (domain <- ports <- adapters/app/interfaces <- bin)
#   3. adapter-specific methods called outside the adapter crate
#   4. application integration tests must not depend on concrete adapters
#   5. unsafe_code is allowed only in the designated FFI boundary crate
#
# dev-dependencies and build-dependencies are excluded from dep checks.

set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v jq >/dev/null 2>&1; then
    echo "check-hexagonal-architecture: jq is required" >&2
    exit 2
fi

# -----------------------------------------------------------------------------
# helpers
# -----------------------------------------------------------------------------

violations=0

warn() {
    echo "VIOLATION: $1" >&2
    violations=$((violations + 1))
}

# workspace crate membership
is_domain()   { [[ "$1" == "mars-types" || "$1" == "mars-grid" || "$1" == "mars-expr" || "$1" == "mars-style" || "$1" == "mars-artifact" ]]; }
is_port()     { [[ "$1" == "mars-source" || "$1" == "mars-store" || "$1" == "mars-render-port" ]]; }
is_adapter()  { [[ "$1" == "mars-source-postgres" || "$1" == "mars-store-s3" || "$1" == "mars-store-fs" || "$1" == "mars-render" ]]; }
is_app()      { [[ "$1" == "mars-compiler" || "$1" == "mars-runtime" ]]; }
is_interface(){ [[ "$1" == "mars-wms" || "$1" == "mars-wmts" || "$1" == "mars-http" ]]; }
is_support()  { [[ "$1" == "mars-config" || "$1" == "mars-observability" || "$1" == "mars-proj" || "$1" == "mars-text" ]]; }

is_bin() { [[ "$1" == "mars" || "$1" == "mars-import-mapfile" || "$1" == "mars-compile" || "$1" == "mars-bin-shared" ]]; }

is_workspace_crate() {
    is_domain "$1" || is_port "$1" || is_adapter "$1" || is_app "$1" || is_interface "$1" || is_support "$1" || is_bin "$1"
}

check_dep_direction() {
    local consumer="$1" dep="$2"

    # rule: a crate may depend on crates in the same layer or any layer below it.
    # lower layers: domain < ports < adapters | application | interfaces < support < bin
    # (support is a sibling to application/interfaces; adapters are a sibling to app/interfaces)

    if is_domain "$consumer"; then
        is_domain "$dep" && return 0
        warn "domain crate '$consumer' has workspace dep on '$dep' (domain may only depend on domain)"
    elif is_port "$consumer"; then
        is_domain "$dep" && return 0
        is_port "$dep"   && return 0
        warn "port crate '$consumer' has workspace dep on '$dep' (ports may only depend on domain, ports)"
    elif is_adapter "$consumer"; then
        is_domain "$dep"  && return 0
        is_port "$dep"    && return 0
        is_support "$dep" && return 0
        warn "adapter crate '$consumer' has workspace dep on '$dep' (adapters may only depend on domain, ports, support)"
    elif is_app "$consumer"; then
        is_domain "$dep"  && return 0
        is_port "$dep"    && return 0
        is_support "$dep" && return 0
        is_app "$dep"     && return 0
        warn "application crate '$consumer' has workspace dep on '$dep' (application may only depend on domain, ports, support, application)"
    elif is_interface "$consumer"; then
        is_domain "$dep"  && return 0
        is_port "$dep"    && return 0
        is_app "$dep"     && return 0
        is_support "$dep" && return 0
        is_interface "$dep" && return 0
        warn "interface crate '$consumer' has workspace dep on '$dep' (interfaces may only depend on domain, ports, application, support, interfaces)"
    elif is_support "$consumer"; then
        is_domain "$dep"  && return 0
        is_support "$dep" && return 0
        warn "support crate '$consumer' has workspace dep on '$dep' (support may only depend on domain, support)"
    fi
}

# -----------------------------------------------------------------------------
# 1. banned runtime / I/O crates in domain / ports
# -----------------------------------------------------------------------------

BANNED=(tokio axum sqlx "object_store" "tiny-skia" hyper reqwest)
BANNED_PREFIXES=("aws-")

is_banned_crate() {
    local name="$1"
    for b in "${BANNED[@]}"; do
        [[ "$name" == "$b" ]] && return 0
    done
    for p in "${BANNED_PREFIXES[@]}"; do
        [[ "$name" == "$p"* ]] && return 0
    done
    return 1
}

is_protected_path() {
    case "$1" in
        */crates/domain/*) return 0 ;;
        */crates/ports/*)  return 0 ;;
        *) return 1 ;;
    esac
}

echo "--- 1. banned runtime crates in domain/ports ---"

while IFS=$'\t' read -r crate manifest dep kind; do
    is_protected_path "$manifest" || continue
    [[ "$kind" == "build" || "$kind" == "dev" ]] && continue
    is_banned_crate "$dep" || continue
    warn "domain/port crate '$crate' has normal-kind dep on banned crate '$dep' (manifest: $manifest)"
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

# -----------------------------------------------------------------------------
# 2. layer dependency direction
# -----------------------------------------------------------------------------

echo "--- 2. layer dependency direction ---"

while IFS=$'\t' read -r crate manifest dep kind; do
    is_workspace_crate "$crate" || continue
    is_workspace_crate "$dep"   || continue
    [[ "$kind" == "build" || "$kind" == "dev" ]] && continue
    check_dep_direction "$crate" "$dep"
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

# -----------------------------------------------------------------------------
# 3. adapter-specific methods outside adapter crates
#    these are methods that exist on concrete adapters but are not defined
#    on the corresponding port trait; calling them leaks adapter details.
# -----------------------------------------------------------------------------

echo "--- 3. adapter-specific methods outside adapter crates ---"

# methods that belong to FsPublisher but not to ManifestPublisher
for method in "read_current(" "manifests_dir(" "new_with_poll_interval("; do
    matches=$(grep -r "$method" crates/ bin/ --include="*.rs" 2>/dev/null) || true
    # keep only matches outside any manifest-store adapter crate
    filtered=$(echo "$matches" | grep -v "crates/adapters/mars-store-fs/" | grep -v "crates/adapters/mars-store-s3/" || true)
    if [[ -n "$filtered" ]]; then
        warn "adapter-specific method '.$method' called outside a manifest-store adapter:"
        echo "$filtered" | sed 's/^/    /'
    fi
done

# -----------------------------------------------------------------------------
# 4. concrete adapter imports outside adapter crates (excluding bin/)
# -----------------------------------------------------------------------------

echo "--- 4. concrete adapter imports outside adapter crates ---"

for adapter_mod in "mars_store_fs" "mars_source_postgres" "mars_render"; do
    matches=$(grep -r "use ${adapter_mod}::" crates/ --include="*.rs" 2>/dev/null) || true
    # map module name back to crate directory
    crate_dir=""
    case "$adapter_mod" in
        mars_store_fs)       crate_dir="crates/adapters/mars-store-fs" ;;
        mars_source_postgres) crate_dir="crates/adapters/mars-source-postgres" ;;
        mars_render)         crate_dir="crates/adapters/mars-render" ;;
    esac
    filtered=$(echo "$matches" | grep -v "${crate_dir}/" || true)
    if [[ -n "$filtered" ]]; then
        warn "concrete adapter 'use ${adapter_mod}::' found outside adapter crate:"
        echo "$filtered" | sed 's/^/    /'
    fi
done

# -----------------------------------------------------------------------------
# 5. application integration tests must not depend on concrete adapters
# -----------------------------------------------------------------------------

echo "--- 5. application integration test adapter coupling ---"

for adapter_mod in "mars_store_fs" "mars_source_postgres" "mars_render"; do
    matches=$(grep -r "use ${adapter_mod}::" crates/application/*/tests/ --include="*.rs" 2>/dev/null) || true
    if [[ -n "$matches" ]]; then
        warn "application integration test imports concrete adapter '${adapter_mod}':"
        echo "$matches" | sed 's/^/    /'
    fi
done

# also flag direct usage of adapter concrete types in application tests
for type_pat in "FsStore" "FsCache" "FsPublisher" "PgSource" "PgConfig" "TinySkiaRenderer"; do
    matches=$(grep -r "$type_pat" crates/application/*/tests/ --include="*.rs" 2>/dev/null) || true
    if [[ -n "$matches" ]]; then
        warn "application integration test references concrete adapter type '$type_pat':"
        echo "$matches" | sed 's/^/    /'
    fi
done

# -----------------------------------------------------------------------------
# 6. unsafe_code scope
# -----------------------------------------------------------------------------

echo "--- 6. unsafe_code scope ---"

# the designated FFI boundary must opt in explicitly
if ! grep -q "#\!\[allow(unsafe_code)\]" crates/support/mars-proj/src/lib.rs 2>/dev/null; then
    warn "mars-proj (the designated FFI boundary) is missing '#![allow(unsafe_code)]'"
fi

# no other crate may have a crate-level or module-level allow for unsafe_code
other_unsafe=$(grep -rl "#!\[allow(unsafe_code)\]" crates/ bin/ --include="*.rs" 2>/dev/null || true)
other_unsafe_filtered=$(echo "$other_unsafe" | grep -v "crates/support/mars-proj/src/lib.rs" | grep -v "crates/adapters/mars-store-fs/src/mmap.rs" || true)
if [[ -n "$other_unsafe_filtered" ]]; then
    warn "crate-level or module-level '#![allow(unsafe_code)]' found outside permitted boundaries:"
    echo "$other_unsafe_filtered" | sed 's/^/    /'
fi

# -----------------------------------------------------------------------------
# summary
# -----------------------------------------------------------------------------

if (( violations > 0 )); then
    echo
    echo "$violations hexagonal architecture violation(s) found." >&2
    echo "see ARCHITECTURE.md §3 (dependency rule) and §3.1 (async-boundary rule)." >&2
    exit 1
fi

echo "hexagonal architecture: ok"
