#!/usr/bin/env bash
# two-stage rustc PGO build for the mars binary.
#
# stage 1: build the criterion benches with -Cprofile-generate, run them so
#          the instrumented binaries dump .profraw files into PGO_DIR.
# stage 2: merge with llvm-profdata and rebuild bin/mars with -Cprofile-use.
#
# the training corpus is one criterion bench per dominant runtime hot loop:
#   mars-artifact/iter          - varint decode + dequantize (~42% cpu)
#   mars-render/draw_encode     - tiny-skia stroke/fill + png/jpeg encode (~49%)
#   mars-proj/transform         - reprojection inner loop
#   mars-runtime/render         - plan/cull/iter call graph (noop renderer)
#   mars-expr/parse_eval        - filter eval at render time
#   mars-compiler/decode        - compile-side wkb path; included for parity
#                                 with the publish path the binary also runs.
# rationale lives in the PGO plan; if a bench is added/removed, update both.
#
# requires: nightly-or-later rustc with PGO support (stable works since 1.37),
#           and llvm-profdata (rustup component add llvm-tools-preview).

set -euo pipefail

cd "$(dirname "$0")/.."

PGO_DIR="${PGO_DIR:-$(pwd)/target/pgo-data}"
# cap parallel rustc/linker jobs. lto=fat + profile-generate instrumentation
# blows past 16 GB on a default -j$(nproc); 2 keeps peak RSS well under that
# while still using both stages' codegen-units=1. override with PGO_JOBS=N.
PGO_JOBS="${PGO_JOBS:-2}"
mkdir -p "$PGO_DIR"
rm -f "$PGO_DIR"/*.profraw "$PGO_DIR"/merged.profdata

# locate llvm-profdata via rustup if not on PATH.
if ! command -v llvm-profdata >/dev/null 2>&1; then
    LLVM_BIN=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's|^host: ||p')/bin
    if [[ -x "$LLVM_BIN/llvm-profdata" ]]; then
        export PATH="$LLVM_BIN:$PATH"
    else
        echo "error: llvm-profdata not found; run 'rustup component add llvm-tools-preview'" >&2
        exit 1
    fi
fi

# format: "<crate>:<bench>" (one entry per training target).
corpus=(
    "mars-artifact:iter"
    "mars-render:draw_encode"
    "mars-proj:transform"
    "mars-runtime:render"
    "mars-expr:parse_eval"
    "mars-compiler:decode"
)

# stage 1: build & run each corpus bench with -Cprofile-generate. the
# instrumented binaries are throwaway, so we drop fat LTO here (overrides
# release-pgo) to keep linker memory in check; stage 2 still gets fat LTO.
# --quick keeps the training pass fast; we only need representative samples
# to drive the optimiser, not full criterion stats.
echo "==> stage 1: build & run corpus benches with -Cprofile-generate=$PGO_DIR (jobs=$PGO_JOBS)"
for spec in "${corpus[@]}"; do
    pkg="${spec%:*}"
    bench="${spec#*:}"
    RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
    CARGO_PROFILE_RELEASE_PGO_LTO=thin \
    CARGO_BUILD_JOBS="$PGO_JOBS" \
        cargo bench --profile release-pgo -p "$pkg" --bench "$bench" -- --quick
done

profraw_count=$(find "$PGO_DIR" -name '*.profraw' | wc -l)
if [[ "$profraw_count" -eq 0 ]]; then
    echo "error: no .profraw files generated; check that the benches actually ran" >&2
    exit 1
fi
echo "==> collected $profraw_count .profraw files"

echo "==> merging profile data with llvm-profdata"
llvm-profdata merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

echo "==> stage 2: rebuild with -Cprofile-use=$PGO_DIR/merged.profdata (jobs=$PGO_JOBS)"
# uncovered functions surface as llvm warnings during stage 2. that's the
# signal we want: it tells us which hot paths the corpus still misses. do
# not silence it without first triaging what's actually missing.
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata" \
CARGO_BUILD_JOBS="$PGO_JOBS" \
    cargo build --profile release-pgo --workspace --bin mars

echo "==> done. PGO-built mars at target/release-pgo/mars"
