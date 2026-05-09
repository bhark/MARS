#!/usr/bin/env bash
# regenerate-forvaltning2-fixture.sh - run mars-import-mapfile against the
# upstream forvaltning2 wms.map and diff its output against the committed
# fixture. The diff is expected to be a strict subset of the strip set
# documented in bin/mars/tests/fixtures/forvaltning2/HANDSTRIP.md.
#
# usage:
#   scripts/regenerate-forvaltning2-fixture.sh [--mapfile PATH] [--write PATH]
#
# pass the mapfile path via $MARS_FORVALTNING2_MAPFILE or --mapfile.
# the upstream mapfile lives in a separate, operator-local repository
# and is not vendored here.
#
# exit codes:
#   0  - importer ran cleanly; deltas printed for review
#   1  - importer error or missing mapfile
#   2  - importer emitted warnings under --strict (unexpected construct)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="${ROOT}/bin/mars/tests/fixtures/forvaltning2/service.yaml"
HANDSTRIP="${ROOT}/bin/mars/tests/fixtures/forvaltning2/HANDSTRIP.md"

mapfile_path="${MARS_FORVALTNING2_MAPFILE:-}"
write_path=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mapfile)
      mapfile_path="$2"
      shift 2
      ;;
    --write)
      write_path="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,16p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -z "${mapfile_path}" ]]; then
  echo "mapfile path required: set MARS_FORVALTNING2_MAPFILE or pass --mapfile PATH" >&2
  exit 1
fi
if [[ ! -f "${mapfile_path}" ]]; then
  echo "mapfile not found: ${mapfile_path}" >&2
  exit 1
fi

cd "${ROOT}"

out="$(mktemp -t mars-import-XXXXXX.yaml)"
warn="$(mktemp -t mars-import-warn-XXXXXX.log)"
trap 'rm -f "${out}" "${warn}"' EXIT

cargo run --quiet -p mars-import-mapfile --locked -- \
  --include-layer Landpolygon \
  --include-layer Soe \
  --include-layer Byomraade \
  --include-layer Vejmidte \
  --include-layer Bygning \
  --include-layer Vandloebsmidte \
  "${mapfile_path}" > "${out}" 2> "${warn}"

if [[ -s "${warn}" ]]; then
  echo "--- importer warnings (expected: METADATA / OUTPUTFORMAT / PROJECTION / FONTSET / LEGEND only) ---"
  cat "${warn}"
  echo
fi

if [[ -n "${write_path}" ]]; then
  cp "${out}" "${write_path}"
  echo "wrote raw importer output to ${write_path}"
fi

echo "--- diff (committed fixture vs importer output) ---"
echo "expected deltas: see ${HANDSTRIP#${ROOT}/}"
echo
if diff -u "${FIXTURE}" "${out}"; then
  echo "no diff. fixture is byte-equal to importer output."
else
  echo
  echo "diff above is the surviving hand-strip set; reconcile against HANDSTRIP.md."
fi
