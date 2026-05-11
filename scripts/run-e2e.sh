#!/usr/bin/env bash
set -euo pipefail

# MARS end-to-end harness over the docker compose stack.
#
# - brings up tests/e2e/compose.yml (postgis + fixture-loader +
#   compiler + runtime)
# - waits on the runtime healthcheck (subsumes the prior /healthz + /readyz
#   polling)
# - runs WMS GetCapabilities + GetMap against the runtime and asserts a
#   PNG response
# - on exit (unless --keep-stack): `compose down -v` removes containers
#   and named volumes
#
# usage: scripts/run-e2e.sh [--fixture PATH] [--keep-stack] [--skip-build]

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

FIXTURE="${MARS_E2E_FIXTURE:-${ROOT}/target/e2e-fixtures/local-map-subset.sql.gz}"
RUNTIME_PORT="${MARS_E2E_RUNTIME_PORT:-18080}"
COMPOSE_FILE="${ROOT}/tests/e2e/compose.yml"
KEEP_STACK="0"
SKIP_BUILD="0"

usage() {
  cat <<EOF
usage: scripts/run-e2e.sh [--fixture PATH] [--keep-stack] [--skip-build]

Runs the MARS e2e harness against the docker compose stack at
tests/e2e/compose.yml. Default fixture path:
target/e2e-fixtures/local-map-subset.sql.gz
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fixture) FIXTURE="$2"; shift 2 ;;
    --keep-stack) KEEP_STACK="1"; shift ;;
    --skip-build) SKIP_BUILD="1"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
need() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 2; }
}

compose() {
  docker compose -f "$COMPOSE_FILE" "$@"
}

cleanup() {
  local status=$?
  if [[ $status -ne 0 ]]; then
    log "diagnostics: compose ps"
    compose ps || true
    log "diagnostics: compose logs (last 200 lines per service)"
    compose logs --tail=200 || true
  fi
  if [[ "$KEEP_STACK" != "1" ]]; then
    compose down -v --remove-orphans >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT

need docker
need curl
docker compose version >/dev/null 2>&1 || {
  echo "docker compose v2 plugin required" >&2; exit 2;
}

if [[ ! -f "$FIXTURE" ]]; then
  echo "fixture dump not found: $FIXTURE" >&2
  echo "see tests/e2e/fixtures/local-map-subset/README.md to produce it" >&2
  exit 2
fi

# bind-mount target reuses the canonical fixture location; symlink any
# user-provided --fixture into place so the compose file can stay static.
DEFAULT_FIXTURE="${ROOT}/target/e2e-fixtures/local-map-subset.sql.gz"
if [[ "$FIXTURE" != "$DEFAULT_FIXTURE" ]]; then
  mkdir -p "$(dirname "$DEFAULT_FIXTURE")"
  ln -sfn "$FIXTURE" "$DEFAULT_FIXTURE"
fi

log "preflight: tearing down any prior e2e stack"
compose down -v --remove-orphans >/dev/null 2>&1 || true

UP_ARGS=(up -d --wait)
if [[ "$SKIP_BUILD" != "1" ]]; then
  UP_ARGS+=(--build)
fi

log "compose: ${UP_ARGS[*]}"
compose "${UP_ARGS[@]}"

log "checks: GetCapabilities"
caps="$(mktemp)"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetCapabilities" -o "$caps"
grep -q "WMS_Capabilities" "$caps"

log "checks: GetMap (PNG magic)"
png="$(mktemp)"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways&styles=&crs=EPSG:25832&bbox=850000,6090000,895000,6145000&width=768&height=768&format=image/png" -o "$png"
head -c 8 "$png" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$'

log "ok: e2e passed"
