#!/usr/bin/env bash
set -euo pipefail

# maintainer-only helper: cut the parity OSM fixture GitHub Release. the
# e2e suite shares this dump (see tests/e2e/scripts/fetch-fixture.sh and
# tests/integration/fixtures/e2e-osm/derive-e2e.sql), so there is exactly
# one public fixture asset.
#
# computes the sha256, writes the in-repo manifest, then calls
# `gh release create` with the asset and ODbL attribution notes.
#
# usage:
#   scripts/release-fixtures.sh v1 --file ~/mars-fixtures/osm-parity.sql.gz
#
# flags:
#   --file PATH    absolute path to the local dump (defaults to
#                  ${HOME}/mars-fixtures/osm-parity.sql.gz)
#   --dry-run      do everything except create the Release. manifest + notes
#                  are still written so they can be reviewed before publishing.
#
# after the Release is live the script prints the suggested commit command
# for the updated manifest. the maintainer runs that, reviews, and pushes.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="bhark/MARS"

usage() {
  cat <<EOF
usage: scripts/release-fixtures.sh <version> [--file PATH] [--dry-run]

  <version>   tag suffix, e.g. v1, v2, v3-rc1

  --file PATH absolute path to the local dump. default:
                \${HOME}/mars-fixtures/osm-parity.sql.gz
  --dry-run   skip the actual 'gh release create' call
EOF
}

VERSION=""
FILE=""
DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --file) FILE="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -*)
      echo "unknown flag: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ -z "${VERSION}" ]]; then VERSION="$1"
      else echo "unexpected positional arg: $1" >&2; usage >&2; exit 2
      fi
      shift
      ;;
  esac
done

if [[ -z "${VERSION}" ]]; then
  usage >&2
  exit 2
fi

FILE_KEY="osm-parity.sql.gz"
MANIFEST="${ROOT}/tests/parity/fixtures/osm/manifest.sha256"
DEFAULT_FILE="${HOME}/mars-fixtures/osm-parity.sql.gz"
TAG_PREFIX="parity-fixtures"
RELEASE_TITLE="Parity fixtures ${VERSION}"

FILE="${FILE:-${DEFAULT_FILE}}"
TAG="${TAG_PREFIX}-${VERSION}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 2; }; }
need gh
need sha256sum
need awk

if [[ ! -f "${FILE}" ]]; then
  echo "fixture file not found: ${FILE}" >&2
  echo "pass --file to point at the local dump." >&2
  exit 2
fi

if [[ ${DRY_RUN} -eq 0 ]]; then
  if ! gh auth status >/dev/null 2>&1; then
    echo "gh is not authenticated; run 'gh auth login' first." >&2
    exit 2
  fi
  if gh release view "${TAG}" --repo "${REPO}" >/dev/null 2>&1; then
    echo "Release '${TAG}' already exists on ${REPO}." >&2
    echo "bump the version (e.g. 'v$(echo "${VERSION}" | sed 's/^v//; s/[^0-9]//g; s/$/+1/' | bc 2>/dev/null || echo N)') and try again." >&2
    exit 2
  fi
fi

SHA="$(sha256sum "${FILE}" | awk '{print $1}')"
URL="https://github.com/${REPO}/releases/download/${TAG}/${FILE_KEY}"

NOTES_FILE="$(mktemp -t mars-fixture-notes.XXXXXX.md)"
trap 'rm -f "${NOTES_FILE}"' EXIT

cat > "${NOTES_FILE}" <<EOF
\`${FILE_KEY}\` contains data (C) OpenStreetMap contributors, available
under the Open Database License (ODbL). See
https://www.openstreetmap.org/copyright for the full license.

Source: Liechtenstein extract, processed with \`osm2pgsql\`, captured as
\`pg_dump --format=plain | gzip\`. Schema is osm2pgsql's native
\`planet_osm_*\` layout. Two consumers layer derived materialised tables
on top:
  - parity: \`tests/parity/fixtures/osm/02-views.sql\` -> \`parity_*\`
  - e2e:    \`tests/integration/fixtures/e2e-osm/derive-e2e.sql\` -> \`e2e_source.*\` (in EPSG:25832)

sha256: \`${SHA}\`
EOF

write_manifest() {
  local target="$1"
  mkdir -p "$(dirname "${target}")"
  cat > "${target}" <<EOF
# source: ${URL}
# tag: ${TAG}
${SHA}  ${FILE_KEY}
EOF
}

echo
echo "--- fixture release plan ---"
printf 'file:      %s\n' "${FILE}"
printf 'sha256:    %s\n' "${SHA}"
printf 'tag:       %s\n' "${TAG}"
printf 'asset url: %s\n' "${URL}"
printf 'manifest:  %s\n' "${MANIFEST}"
printf 'notes:     %s\n' "${NOTES_FILE}"
echo "----------------------------"
echo

write_manifest "${MANIFEST}"
printf 'wrote manifest -> %s\n' "${MANIFEST}"

if [[ ${DRY_RUN} -eq 1 ]]; then
  printf '\n[dry-run] skipping gh release create.\n'
  printf 'review the manifest + notes, then re-run without --dry-run.\n'
  cat "${NOTES_FILE}"
  exit 0
fi

printf '\ncreating Release %s on %s...\n' "${TAG}" "${REPO}"
gh release create "${TAG}" "${FILE}" \
  --repo "${REPO}" \
  --title "${RELEASE_TITLE}" \
  --notes-file "${NOTES_FILE}"

cat <<EOF

Release published: https://github.com/${REPO}/releases/tag/${TAG}

next: commit the updated manifest. suggested:

  git add ${MANIFEST}
  git commit -m "chore(parity): pin osm fixture to ${TAG}"

then run scripts/run-parity.sh and scripts/run-e2e.sh once to verify the
SHA round-trips against the published asset. if it doesn't, re-run this
script with a bumped version (the published asset is byte-pinned; never
overwrite an existing tag's asset).
EOF
