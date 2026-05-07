#!/usr/bin/env bash
# two-stage rustc PGO build for the mars binary.
#
# stage 1: collect .profraw counters from one or both training corpora.
# stage 2: merge with llvm-profdata and rebuild bin/mars with -Cprofile-use.
#
# corpora (selected via --source):
#   bench  - criterion benches across the workspace; cheap, deterministic,
#            covers tight inner loops at high iteration counts. one bench per
#            dominant runtime hot loop:
#              mars-artifact/iter        - varint decode + dequantize
#              mars-render/draw_encode   - tiny-skia stroke/fill + encode
#              mars-proj/transform       - reprojection inner loop
#              mars-runtime/render       - plan/cull/iter call graph
#              mars-expr/parse_eval      - filter eval at render time
#              mars-compiler/decode      - compile-side wkb path
#   diff   - profraws collected out-of-band by an instrumented diff-capture
#            run against a real PostGIS workload. drives realistic call
#            graphs + inlining decisions through the runtime render path.
#            this script does not collect them; it expects them already
#            placed under target/pgo-data/diff-harness/.
#   both   - default; merge both sets so stage 2 sees union coverage.
#
# layout under target/pgo-data/:
#   bench/         stage-1 bench profraws
#   diff-harness/  stage-1 diff-harness profraws
#   merged.profdata
#
# requires: rustc with PGO support (stable since 1.37) and llvm-profdata
#           (rustup component add llvm-tools-preview).

set -euo pipefail

cd "$(dirname "$0")/.."

PGO_DIR="${PGO_DIR:-$(pwd)/target/pgo-data}"
# cap parallel rustc/linker jobs. lto=fat + profile-generate instrumentation
# blows past 16 GB on a default -j$(nproc); 2 keeps peak RSS well under that
# while still using both stages' codegen-units=1. override with PGO_JOBS=N.
PGO_JOBS="${PGO_JOBS:-2}"

source="both"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --source)
            source="$2"; shift 2 ;;
        --source=*)
            source="${1#*=}"; shift ;;
        -h|--help)
            sed -n '2,30p' "$0"; exit 0 ;;
        *)
            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

case "$source" in
    bench|diff|both) ;;
    *) echo "error: --source must be one of: bench, diff, both (got: $source)" >&2; exit 2 ;;
esac

mkdir -p "$PGO_DIR/bench" "$PGO_DIR/diff-harness"
rm -f "$PGO_DIR"/*.profraw \
      "$PGO_DIR/bench"/*.profraw \
      "$PGO_DIR/merged.profdata"
# diff-harness profraws are produced by a separate run; only wipe them when
# stage-1 will repopulate them (i.e. when caller is going to re-collect).
if [[ "$source" == "bench" ]]; then
    : # leave existing diff-harness profraws untouched; they won't be used
fi

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

# stage 1a: bench corpus.
if [[ "$source" == "bench" || "$source" == "both" ]]; then
    echo "==> stage 1 (bench): build & run corpus benches with -Cprofile-generate=$PGO_DIR/bench (jobs=$PGO_JOBS)"
    for spec in "${corpus[@]}"; do
        pkg="${spec%:*}"
        bench="${spec#*:}"
        RUSTFLAGS="-Cprofile-generate=$PGO_DIR/bench" \
        CARGO_PROFILE_RELEASE_PGO_LTO=thin \
        CARGO_BUILD_JOBS="$PGO_JOBS" \
            cargo bench --profile release-pgo -p "$pkg" --bench "$bench" -- --quick
    done
    bench_count=$(find "$PGO_DIR/bench" -name '*.profraw' | wc -l)
    if [[ "$bench_count" -eq 0 ]]; then
        echo "error: no bench .profraw files generated; check that the benches actually ran" >&2
        exit 1
    fi
    echo "==> bench corpus: $bench_count .profraw files"
fi

# stage 1b: diff-harness corpus. collected out-of-band by the local diff-capture
# tooling; this script only validates that profraws are present.
if [[ "$source" == "diff" || "$source" == "both" ]]; then
    diff_count=$(find "$PGO_DIR/diff-harness" -name '*.profraw' 2>/dev/null | wc -l)
    if [[ "$diff_count" -eq 0 ]]; then
        echo "error: no diff-harness .profraw files in $PGO_DIR/diff-harness" >&2
        echo "       collect them first with the local diff-capture tooling (--profile-generate mode)" >&2
        exit 1
    fi
    echo "==> diff-harness corpus: $diff_count .profraw files"
fi

echo "==> merging profile data with llvm-profdata"
# llvm-profdata merge walks subdirs recursively, so passing $PGO_DIR picks up
# whichever of bench/ and diff-harness/ contain profraws.
llvm-profdata merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

echo "==> stage 2: rebuild with -Cprofile-use=$PGO_DIR/merged.profdata (jobs=$PGO_JOBS)"
# uncovered functions surface as llvm warnings during stage 2. that's the
# signal we want: it tells us which hot paths the corpus still misses. do
# not silence it without first triaging what's actually missing.
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata" \
CARGO_BUILD_JOBS="$PGO_JOBS" \
    cargo build --profile release-pgo --workspace --bin mars

echo "==> done. PGO-built mars at target/release-pgo/mars (source=$source)"
