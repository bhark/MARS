#!/usr/bin/env bash
set -euo pipefail

# MARS end-to-end harness over the Podman Quadlet stack.
#
# - renders manifests/overlays/e2e via kubectl kustomize, splits per
#   `app.kubernetes.io/part-of` into ~/.local/share/mars-e2e/manifests
# - symlinks tests/e2e/quadlets/* into ~/.config/containers/systemd/mars-e2e
# - populates the `mars-fixture` podman volume with the gzipped SQL dump
# - starts the unit chain (config -> postgis -> fixture-loader -> compiler
#   -> runtime); waits on /healthz then /readyz; runs WMS GetCapabilities
#   + GetMap and asserts a PNG response
# - on failure: dumps systemctl/journal/pod diagnostics
# - on success or failure (unless --keep-stack): tears down units, removes
#   manifests, removes podman volumes
#
# usage: scripts/run-e2e.sh [--fixture PATH] [--keep-stack] [--skip-build]

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

FIXTURE="${MARS_E2E_FIXTURE:-${ROOT}/target/e2e-fixtures/local-map-subset.sql.gz}"
RUNTIME_PORT="${MARS_E2E_RUNTIME_PORT:-18080}"
IMAGE="${MARS_E2E_IMAGE:-localhost/mars:e2e}"
KEEP_STACK="0"
SKIP_BUILD="0"

QUADLET_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/mars-e2e"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mars-e2e"
MANIFEST_DIR="$DATA_DIR/manifests"
QUADLET_SRC="$ROOT/tests/e2e/quadlets"
OVERLAY_DIR="$ROOT/manifests/overlays/e2e"

UNITS=(
  mars-e2e-runtime.service
  mars-e2e-compiler.service
  mars-e2e-fixture-loader.service
  mars-e2e-postgis.service
  mars-e2e-config.service
)
VOLUMES=(
  e2e-mars-postgis-data
  e2e-mars-artifact-store
  e2e-mars-compiler-cache
  e2e-mars-compiler-work
  e2e-mars-runtime-cache
  e2e-mars-fixture
)

usage() {
  cat <<EOF
usage: scripts/run-e2e.sh [--fixture PATH] [--keep-stack] [--skip-build]

Runs the MARS e2e harness against the Quadlet stack
(tests/e2e/quadlets + manifests/overlays/e2e). Default fixture path:
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

dump_diagnostics() {
  log "diagnostics: unit status"
  systemctl --user status 'mars-e2e-*' --no-pager --lines=30 || true
  log "diagnostics: journal (last 10m)"
  journalctl --user -u 'mars-e2e-*' --since=-10m --no-pager || true
  log "diagnostics: pods/containers"
  podman ps -a --filter label=app.kubernetes.io/name=mars || true
}

teardown_stack() {
  # glob-stop catches the Quadlet-generated mars-e2e-network.service too,
  # which UNITS= deliberately omits (it's wired implicitly via `Network=`
  # in the .kube units). leaving it active across runs hides its unit-file
  # deletion from systemd and breaks the next pod start with `network not
  # found`.
  systemctl --user stop 'mars-e2e-*.service' 2>/dev/null || true
}

remove_install() {
  if [[ -d "$QUADLET_DIR" ]]; then
    find "$QUADLET_DIR" -maxdepth 1 -type l -delete 2>/dev/null || true
    rmdir "$QUADLET_DIR" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
  systemctl --user daemon-reload
}

remove_volumes() {
  for v in "${VOLUMES[@]}"; do
    podman volume rm -f "$v" >/dev/null 2>&1 || true
  done
  podman network rm -f mars-e2e >/dev/null 2>&1 || true
}

cleanup() {
  local status=$?
  if [[ $status -ne 0 ]]; then
    dump_diagnostics
  fi
  if [[ "$KEEP_STACK" != "1" ]]; then
    teardown_stack
    remove_install
    remove_volumes
  fi
  exit "$status"
}
trap cleanup EXIT

need podman
need kubectl
need yq
need curl
need systemctl
if ! yq --version 2>&1 | grep -qE 'mikefarah|version (4|v4)'; then
  echo "yq must be mikefarah/yq v4+ (got: $(yq --version 2>&1 | head -1))" >&2
  exit 2
fi

if [[ ! -f "$FIXTURE" ]]; then
  echo "fixture dump not found: $FIXTURE" >&2
  echo "see tests/e2e/fixtures/local-map-subset/README.md to produce it" >&2
  exit 2
fi

log "preflight: stopping any prior e2e stack"
teardown_stack

log "build: $IMAGE"
if [[ "$SKIP_BUILD" != "1" ]]; then
  podman build -t "$IMAGE" "$ROOT"
fi

log "install: rendering overlay"
mkdir -p "$QUADLET_DIR" "$MANIFEST_DIR"
RENDERED="$DATA_DIR/rendered.yaml"
kubectl kustomize --load-restrictor=LoadRestrictionsNone "$OVERLAY_DIR" > "$RENDERED"

for app in config postgis fixture-loader compiler runtime; do
  yq ea \
    "select(.metadata.labels.\"app.kubernetes.io/part-of\" == \"$app\")" \
    "$RENDERED" > "$MANIFEST_DIR/$app.yaml"
  if ! [[ -s "$MANIFEST_DIR/$app.yaml" ]]; then
    echo "install: rendered manifest for '$app' is empty" >&2
    exit 1
  fi
done

# extract mars-config so compiler/runtime .kube units can pre-load it via
# Quadlet ConfigMap= (podman kube play does not persist ConfigMaps across
# plays). mirrors dev/install.sh.
yq ea \
  'select(.kind == "ConfigMap" and .metadata.name == "e2e-mars-config")' \
  "$RENDERED" > "$MANIFEST_DIR/mars-config-cm.yaml"
if ! [[ -s "$MANIFEST_DIR/mars-config-cm.yaml" ]]; then
  echo "install: extracted mars-config ConfigMap is empty" >&2
  exit 1
fi

for f in "$QUADLET_SRC"/*; do
  ln -sfn "$f" "$QUADLET_DIR/$(basename "$f")"
done

systemctl --user daemon-reload

log "fixture: populating e2e-mars-fixture volume from $FIXTURE"
podman volume rm -f e2e-mars-fixture >/dev/null 2>&1 || true
podman volume create e2e-mars-fixture >/dev/null
podman run --rm \
  -v e2e-mars-fixture:/dst \
  -v "$FIXTURE":/src/dump.sql.gz:ro,Z \
  docker.io/alpine:latest \
  cp /src/dump.sql.gz /dst/dump.sql.gz

log "start: mars-e2e-runtime.service (chain pulls dependencies)"
systemctl --user start mars-e2e-runtime.service

# fixture loader is oneshot + ExitCodePropagation=all; surface its result
# as an explicit gate before waiting on the runtime endpoints.
log "wait: fixture loader"
for _ in $(seq 1 120); do
  state="$(systemctl --user is-active mars-e2e-fixture-loader.service 2>/dev/null || true)"
  case "$state" in
    active) break ;;
    failed) echo "fixture-loader failed" >&2; exit 1 ;;
  esac
  sleep 2
done

log "wait: runtime /healthz"
for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/healthz" >/dev/null

log "wait: runtime /readyz (transitively asserts manifest published)"
for _ in $(seq 1 180); do
  if curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/readyz" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/readyz" >/dev/null

log "checks: GetCapabilities"
caps="$(mktemp)"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetCapabilities" -o "$caps"
grep -q "WMS_Capabilities" "$caps"

log "checks: GetMap (PNG magic)"
png="$(mktemp)"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways&styles=&crs=EPSG:25832&bbox=850000,6090000,895000,6145000&width=768&height=768&format=image/png" -o "$png"
head -c 8 "$png" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$'

log "ok: e2e passed"
