#!/usr/bin/env bash
set -euo pipefail

# MARS kind-based e2e harness. lifecycle is owned here; the rust test driver
# (tests/e2e/) only drives in-cluster state once everything is wired up.
#
# - builds both `mars` and `mars-operator` images locally
# - creates a kind cluster (`mars-e2e`) and loads the images into it
# - helm-installs the operator chart into `mars-operator-system`
# - runs `cargo test --release` against tests/e2e
# - on failure: dumps logs + descriptions of all pods in test namespaces +
#   the operator namespace into target/e2e-output/<run-id>/
# - on exit: deletes the cluster unless MARS_E2E_KEEP=1
#
# usage: scripts/run-e2e.sh [--skip-image-build] [--no-fetch] [--test FILTER]

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLUSTER="${MARS_E2E_CLUSTER:-mars-e2e}"
OPERATOR_NS="${MARS_E2E_OPERATOR_NS:-mars-operator-system}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
OUT_DIR="${ROOT}/target/e2e-output/${RUN_ID}"

SKIP_BUILD=0
NO_FETCH=0
TEST_FILTER=""

usage() {
  cat <<EOF
usage: scripts/run-e2e.sh [--skip-image-build] [--no-fetch] [--test FILTER]

env knobs:
  MARS_E2E_KEEP=1            leave the kind cluster + test namespaces alive
  MARS_E2E_FIXTURE_URL=URL   fetch the public test dataset from URL
  MARS_E2E_FIXTURE_PATH=P    bypass fetch + use existing dump at P
  MARS_E2E_CLUSTER=name      kind cluster name (default: mars-e2e)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-image-build) SKIP_BUILD=1; shift ;;
    --no-fetch) NO_FETCH=1; shift ;;
    --test) TEST_FILTER="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 2; }; }

need docker
need kind
need helm
need kubectl
need cargo

mkdir -p "${OUT_DIR}"

# tee everything into run.log so the artifact has something useful even when
# we bail before a kind cluster exists (missing fixture, missing tool, etc).
exec > >(tee -a "${OUT_DIR}/run.log") 2>&1

dump_diagnostics() {
  log "diagnostics -> ${OUT_DIR}"
  kubectl get ns -o name 2>/dev/null | while read -r ns_ref; do
    ns="${ns_ref#namespace/}"
    case "${ns}" in
      mars-e2e-*|${OPERATOR_NS})
        mkdir -p "${OUT_DIR}/${ns}"
        kubectl -n "${ns}" get all -o wide > "${OUT_DIR}/${ns}/get-all.txt" 2>&1 || true
        kubectl -n "${ns}" describe pods > "${OUT_DIR}/${ns}/describe-pods.txt" 2>&1 || true
        kubectl -n "${ns}" get events --sort-by=.lastTimestamp > "${OUT_DIR}/${ns}/events.txt" 2>&1 || true
        kubectl -n "${ns}" get pods -o name 2>/dev/null | while read -r pod_ref; do
          pod="${pod_ref#pod/}"
          kubectl -n "${ns}" logs --all-containers "${pod}" > "${OUT_DIR}/${ns}/${pod}.log" 2>&1 || true
        done
        ;;
    esac
  done
}

cleanup() {
  local status=$?
  if [[ ${status} -ne 0 ]]; then
    dump_diagnostics || true
  fi
  if [[ "${MARS_E2E_KEEP:-0}" != "1" ]]; then
    log "tearing down kind cluster ${CLUSTER}"
    kind delete cluster --name "${CLUSTER}" >/dev/null 2>&1 || true
  else
    log "MARS_E2E_KEEP=1 set; leaving kind cluster ${CLUSTER} up"
  fi
  printf 'exit_status=%d\nrun_id=%s\n' "${status}" "${RUN_ID}" > "${OUT_DIR}/status.txt"
  exit "${status}"
}
trap cleanup EXIT

if [[ "${NO_FETCH}" != "1" ]]; then
  log "ensuring fixture dump is present"
  "${ROOT}/tests/e2e/scripts/fetch-fixture.sh"
fi

# operator vX.Y.Z runs mars vX.Y.Z (chart appVersion == operator
# CARGO_PKG_VERSION). pin the e2e image tag to the workspace version so
# kind-loaded images resolve under the same name the chart picks.
WORKSPACE_VERSION="$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "${ROOT}/Cargo.toml")"
[[ -n "${WORKSPACE_VERSION}" ]] || { echo "could not read workspace version from Cargo.toml" >&2; exit 2; }
MARS_TAG="${WORKSPACE_VERSION}"
MARS_IMAGE="localhost/mars:${MARS_TAG}"
OPERATOR_IMAGE="localhost/mars-operator:${MARS_TAG}"

if [[ "${SKIP_BUILD}" != "1" ]]; then
  log "building mars + mars-operator images at ${MARS_TAG}"
  docker build --build-arg BIN=mars          -t "${MARS_IMAGE}"     "${ROOT}"
  docker build --build-arg BIN=mars-operator -t "${OPERATOR_IMAGE}" "${ROOT}"
fi

if ! kind get clusters | grep -qx "${CLUSTER}"; then
  log "creating kind cluster ${CLUSTER}"
  # render kind.yaml.tmpl with an absolute hostPath; kind resolves relative
  # extraMounts against the invoker's cwd, so the template lets the script be
  # run from anywhere without misplacing the fixture mount.
  KIND_CFG="$(mktemp -t mars-e2e-kind.XXXXXX.yaml)"
  sed "s|{{REPO_ROOT}}|${ROOT}|g" "${ROOT}/tests/e2e/kind.yaml.tmpl" > "${KIND_CFG}"
  kind create cluster --name "${CLUSTER}" --config "${KIND_CFG}"
  rm -f "${KIND_CFG}"
else
  log "kind cluster ${CLUSTER} already exists; reusing"
fi

log "loading images into kind"
kind load docker-image --name "${CLUSTER}" "${MARS_IMAGE}" "${OPERATOR_IMAGE}"

# pin chart appVersion to the workspace version so the chart-rendered
# operator-image tag matches what kind has loaded.
log "installing mars-operator chart into ${OPERATOR_NS} (appVersion=${MARS_TAG})"
helm upgrade --install mars-operator "${ROOT}/charts/mars-operator" \
  --namespace "${OPERATOR_NS}" --create-namespace \
  --values "${ROOT}/tests/e2e/manifests/operator-values.yaml" \
  --set-string image.tag="${MARS_TAG}" \
  --wait --timeout 5m

log "running rust e2e suite (tests/e2e)"
(
  cd "${ROOT}/tests/e2e"
  export MARS_E2E_OPERATOR_NS="${OPERATOR_NS}"
  if [[ -n "${TEST_FILTER}" ]]; then
    cargo test --release --test e2e_suite -- --nocapture --test-threads=1 "${TEST_FILTER}"
  else
    cargo test --release --test e2e_suite -- --nocapture --test-threads=1
  fi
)

log "ok: e2e passed"
