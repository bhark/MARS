#!/usr/bin/env bash
set -euo pipefail

# fetch the public test data dump into target/e2e-fixtures/. the kind suite
# mounts this dir into the cluster via the extra-mount declared in kind.yaml.tmpl.
#
# hosting: configure `MARS_E2E_FIXTURE_URL` to a maintainer-hosted github-
# releases asset (or any HTTP/HTTPS url). a SHA256 is verified against
# tests/e2e/fixtures/manifest.sha256.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
DEST_DIR="${ROOT}/target/e2e-fixtures"
DEST="${DEST_DIR}/local-map-subset.sql.gz"
MANIFEST="${ROOT}/tests/e2e/fixtures/manifest.sha256"
URL="${MARS_E2E_FIXTURE_URL:-}"

mkdir -p "${DEST_DIR}"

if [[ -f "${DEST}" ]] && [[ -f "${MANIFEST}" ]]; then
  expected="$(awk '/local-map-subset\.sql\.gz/ {print $1}' "${MANIFEST}" || true)"
  actual="$(sha256sum "${DEST}" | awk '{print $1}')"
  if [[ -n "${expected}" && "${expected}" == "${actual}" ]]; then
    echo "fixture present and verified: ${DEST}"
    exit 0
  fi
fi

if [[ -z "${URL}" ]]; then
  cat >&2 <<EOF
fetch-fixture: MARS_E2E_FIXTURE_URL is unset.
The kind e2e suite needs a representative multi-layer dataset that
intentionally is not committed to git. Provide a URL via the env var,
or place the dump manually at:
  ${DEST}
See tests/integration/fixtures/local-map-subset/README.md for the
required shape.
EOF
  exit 2
fi

echo "fetching fixture from ${URL}"
curl -fL --retry 3 --retry-delay 2 -o "${DEST}.partial" "${URL}"
mv "${DEST}.partial" "${DEST}"

if [[ -f "${MANIFEST}" ]]; then
  expected="$(awk '/local-map-subset\.sql\.gz/ {print $1}' "${MANIFEST}" || true)"
  if [[ -n "${expected}" ]]; then
    actual="$(sha256sum "${DEST}" | awk '{print $1}')"
    if [[ "${expected}" != "${actual}" ]]; then
      echo "fetch-fixture: sha256 mismatch (expected ${expected}, got ${actual})" >&2
      exit 1
    fi
    echo "verified sha256: ${actual}"
  fi
fi

echo "fixture ready: ${DEST}"
