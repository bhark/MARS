#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NS="${MARS_E2E_NAMESPACE:-mars-e2e}"
CLUSTER="${MARS_E2E_CLUSTER:-mars-e2e}"
IMAGE="${MARS_E2E_IMAGE:-mars:e2e}"
FIXTURE="${MARS_E2E_FIXTURE:-${ROOT}/target/e2e-fixtures/local-map-subset.sql.gz}"
K3D_MANIFESTS="${ROOT}/tests/e2e/k3d"
FIXTURE_DIR="${ROOT}/tests/e2e/fixtures/local-map-subset"
RUNTIME_PORT="${MARS_E2E_RUNTIME_PORT:-18080}"
KEEP_CLUSTER="0"
SKIP_BUILD="0"

usage() {
  sed -n '2,38p' "$0"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fixture)
      FIXTURE="$2"
      shift 2
      ;;
    --keep-cluster)
      KEEP_CLUSTER="1"
      shift
      ;;
    --skip-build)
      SKIP_BUILD="1"
      shift
      ;;
    -h|--help)
      cat <<EOF
usage: scripts/run-k3d-e2e.sh [--fixture PATH] [--keep-cluster] [--skip-build]

Runs the local Kubernetes e2e harness against a prepared PostGIS dump.
Default dump path: target/e2e-fixtures/local-map-subset.sql.gz
EOF
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

log() {
  printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"
}

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

dump_diagnostics() {
  log "diagnostics: cluster state"
  kubectl get pods,svc,deploy,job,garagecluster,garagebucket,garagekey -n "${NS}" -o wide || true
  log "diagnostics: recent events"
  kubectl get events -n "${NS}" --sort-by=.lastTimestamp || true
  log "diagnostics: compiler logs"
  kubectl logs -n "${NS}" deploy/mars-compiler --tail=200 || true
  log "diagnostics: runtime logs"
  kubectl logs -n "${NS}" deploy/mars-runtime --all-containers --tail=200 || true
  log "diagnostics: preflight logs"
  kubectl logs -n "${NS}" job/s3-preflight --tail=200 || true
  log "diagnostics: manifest check logs"
  kubectl logs -n "${NS}" job/manifest-current-check --tail=200 || true
}

cleanup() {
  local status=$?
  if [[ ${status} -ne 0 ]]; then
    dump_diagnostics
  fi
  if [[ -n "${PORT_FORWARD_PID:-}" ]]; then
    kill "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${KEEP_CLUSTER}" != "1" ]]; then
    k3d cluster delete "${CLUSTER}" >/dev/null 2>&1 || true
  fi
  exit "${status}"
}
trap cleanup EXIT

need docker
need curl
need gzip
need helm
need k3d
need kubectl

if [[ ! -f "${FIXTURE}" ]]; then
  echo "fixture dump not found: ${FIXTURE}" >&2
  echo "prepare a gzip-compressed SQL dump matching ${FIXTURE_DIR#${ROOT}/}/README.md" >&2
  exit 2
fi

if k3d cluster list "${CLUSTER}" >/dev/null 2>&1; then
  log "cluster: deleting existing ${CLUSTER}"
  k3d cluster delete "${CLUSTER}"
fi

log "cluster: creating ${CLUSTER}"
k3d cluster create "${CLUSTER}" --agents 1 --wait

log "garage: installing operator"
helm upgrade --install garage-operator oci://ghcr.io/rajsinghtech/charts/garage-operator \
  --namespace garage-operator-system \
  --create-namespace \
  --set replicaCount=1 \
  --wait \
  --timeout 5m

log "mars: building image ${IMAGE}"
if [[ "${SKIP_BUILD}" != "1" ]]; then
  docker build -t "${IMAGE}" "${ROOT}"
fi
k3d image import "${IMAGE}" -c "${CLUSTER}"

log "k8s: applying base dependencies"
kubectl apply -f "${K3D_MANIFESTS}/namespace.yaml"
kubectl apply -f "${K3D_MANIFESTS}/postgis.yaml"
kubectl apply -f "${K3D_MANIFESTS}/garage.yaml"
kubectl -n "${NS}" wait --for=condition=ready pod -l app.kubernetes.io/name=postgis --timeout=180s
kubectl -n "${NS}" wait --for=condition=Ready garagecluster/mars-e2e-garage --timeout=300s
kubectl -n "${NS}" wait --for=condition=Ready garagebucket/mars-e2e-artifacts --timeout=180s
kubectl -n "${NS}" wait --for=condition=Ready garagekey/mars-e2e-artifacts-writer --timeout=180s
kubectl -n "${NS}" get secret mars-e2e-artifacts-writer >/dev/null

log "fixture: loading ${FIXTURE}"
kubectl apply -f "${K3D_MANIFESTS}/fixture-loader.yaml"
kubectl -n "${NS}" wait --for=condition=ready pod/fixture-loader --timeout=120s
kubectl -n "${NS}" cp "${FIXTURE}" fixture-loader:/tmp/fixture.sql.gz
kubectl -n "${NS}" exec fixture-loader -- sh -c '
  set -eu
  until pg_isready -h postgis -U mars -d mars; do sleep 1; done
  psql "$PG_DSN" -v ON_ERROR_STOP=1 -c "CREATE EXTENSION IF NOT EXISTS postgis"
  gzip -dc /tmp/fixture.sql.gz | psql "$PG_DSN" -v ON_ERROR_STOP=1
  psql "$PG_DSN" -v ON_ERROR_STOP=1 -f /fixture/assert-fixture.sql
  psql "$PG_DSN" -v ON_ERROR_STOP=1 -f /fixture/create-replication.sql
'

log "s3: running compatibility preflight"
kubectl apply -f "${K3D_MANIFESTS}/s3-preflight-job.yaml"
kubectl -n "${NS}" wait --for=condition=complete job/s3-preflight --timeout=180s

log "mars: applying service config and deployments"
kubectl -n "${NS}" create configmap mars-service-config \
  --from-file=mars.yaml="${FIXTURE_DIR}/service.yaml" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f "${K3D_MANIFESTS}/mars.yaml"
kubectl -n "${NS}" set image deploy/mars-compiler mars="${IMAGE}"
kubectl -n "${NS}" set image deploy/mars-runtime mars="${IMAGE}"
kubectl -n "${NS}" rollout status deploy/mars-compiler --timeout=120s

log "mars: waiting for published manifest"
kubectl apply -f "${K3D_MANIFESTS}/manifest-current-check-job.yaml"
kubectl -n "${NS}" wait --for=condition=complete job/manifest-current-check --timeout=900s

log "mars: waiting for runtime replicas"
kubectl -n "${NS}" rollout status deploy/mars-runtime --timeout=300s

log "checks: port-forward runtime service"
kubectl -n "${NS}" port-forward svc/mars-runtime "${RUNTIME_PORT}:8080" >/tmp/mars-e2e-port-forward.log 2>&1 &
PORT_FORWARD_PID=$!
for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/healthz" >/dev/null; then
    break
  fi
  sleep 1
done

curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/healthz" >/dev/null
for _ in $(seq 1 180); do
  if curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/readyz" >/dev/null; then
    break
  fi
  sleep 2
done
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/readyz" >/dev/null

caps="$(mktemp)"
png="$(mktemp)"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetCapabilities" -o "${caps}"
grep -q "WMS_Capabilities" "${caps}"
curl -fsS "http://127.0.0.1:${RUNTIME_PORT}/wms?service=WMS&version=1.3.0&request=GetMap&layers=land,water,settlements,roads,buildings,waterways&styles=&crs=EPSG:25832&bbox=850000,6090000,895000,6145000&width=768&height=768&format=image/png" -o "${png}"
head -c 8 "${png}" | od -An -tx1 | tr -d ' \n' | grep -qi '^89504e470d0a1a0a$'

ready_replicas="$(kubectl -n "${NS}" get deploy mars-runtime -o jsonpath='{.status.readyReplicas}')"
if [[ "${ready_replicas}" != "2" ]]; then
  echo "expected 2 ready runtime replicas, got ${ready_replicas:-0}" >&2
  exit 1
fi

log "ok: k3d e2e passed"
