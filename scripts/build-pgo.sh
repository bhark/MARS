#!/usr/bin/env bash
# two-stage rustc PGO build for the mars binary.
#
# stage 1: build the criterion benches with -Cprofile-generate, run them so
#          the instrumented binaries dump .profraw files into PGO_DIR.
# stage 2: merge with llvm-profdata and rebuild bin/mars with -Cprofile-use.
#
# the criterion benches (mars-artifact, mars-compiler, mars-runtime) form the
# training corpus. they exercise the same hot loops the e2e harness does
# (decode + encode + project + render) without needing docker, which the e2e
# image-diff harness requires.
#
# requires: nightly-or-later rustc with PGO support (stable works since 1.37),
#           and llvm-profdata (rustup component add llvm-tools-preview).

set -euo pipefail

cd "$(dirname "$0")/.."

PGO_DIR="${PGO_DIR:-$(pwd)/target/pgo-data}"
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

echo "==> stage 1: build benches with -Cprofile-generate=$PGO_DIR"
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
    cargo build --profile release-pgo --workspace --benches

echo "==> stage 1: run benches to dump .profraw"
# --quick keeps the training pass fast; we only need representative samples
# to drive the optimiser, not full criterion stats.
for bench in iter decode render; do
    pkg=""
    case "$bench" in
        iter) pkg="mars-artifact" ;;
        decode) pkg="mars-compiler" ;;
        render) pkg="mars-runtime" ;;
    esac
    RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
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

echo "==> stage 2: rebuild with -Cprofile-use=$PGO_DIR/merged.profdata"
# -Cllvm-args=-pgo-warn-missing-function silenced because helper crates with
# zero coverage in the bench corpus are expected (e.g. wmts paths).
RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata -Cllvm-args=-pgo-warn-missing-function=false" \
    cargo build --profile release-pgo --workspace --bin mars

echo "==> done. PGO-built mars at target/release-pgo/mars"
