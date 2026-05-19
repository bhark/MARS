# mars-operator

A Kubernetes operator that reconciles `MarsService` custom resources into a
ConfigMap, the compiler/runtime Deployments, their PVCs, and the runtime
Service. One CR = one logical MARS service in a namespace. Operator scope is
cluster-wide; CRs are namespaced.

## What this chart installs

- The `mars-operator` Deployment (single binary, leader-elected, runs as
  non-root user `65532`).
- A ClusterRole/Binding granting the operator the watches and writes it needs
  on `MarsService`, child Deployments/Services/ConfigMaps/PVCs, leader-election
  Leases, and Events.
- The `MarsService` CRD (apiVersion `mars.forn.dk/v1alpha1`).
- A `ClusterIP` Service exposing `/metrics` on port `9090`.

The CRD is annotated `helm.sh/resource-policy: keep`. `helm uninstall` leaves
it (and any `MarsService` objects) intact. Delete it explicitly with
`kubectl delete crd marsservices.mars.forn.dk` if needed.

## Install

```sh
helm install mars-operator charts/mars-operator \
  --namespace mars-system --create-namespace
```

For GitOps-managed CRDs (Argo, Flux) pass `--skip-crds` and apply
`charts/mars-operator/templates/crd.yaml` separately:

```sh
kubectl apply -f charts/mars-operator/templates/crd.yaml
helm install mars-operator charts/mars-operator \
  --namespace mars-system --skip-crds
```

## Apply a MarsService

The compiler watches PostGIS and publishes versioned artifacts to an object
store; runtime replicas read those artifacts. Two store backends are
supported: `s3` (production) and `fs` (single-node, dev).

```sh
kubectl -n maps create secret generic mars-postgis \
  --from-literal=PG_DSN='postgres://user:pass@postgis:5432/maps'

kubectl apply -f charts/mars-operator/examples/marsservice-s3.yaml
kubectl -n maps get marsservices
```

See `examples/marsservice-s3.yaml` (object-store-backed),
`examples/marsservice-fs.yaml` (single-replica, PVC-backed), or
`examples/marsservice-cnpg.yaml` (operator-driven postgres bootstrap
consuming a K8s-native Postgres operator's Secret directly - zero
user-managed Secrets in the common case).

## Upgrading

`helm upgrade` reapplies operator manifests *and* the CRD (we ship the CRD
under `templates/`, not `crds/`, exactly so this works). The
`resource-policy: keep` annotation means downgrade-via-uninstall does not
delete the CRD.

The CRD is regenerated from `bin/mars-operator/src/crd.rs` via
`scripts/generate-crd.sh`; CI fails on drift.

## Constraints worth knowing

- A `MarsServiceCluster` whose `artifactStore.store.type: fs` paired with a
  `MarsService.spec.runtime.replicas > 1` requires the artifact-store PVC to
  be ReadWriteMany. The operator currently provisions a ReadWriteOnce PVC and
  surfaces a `Degraded=True` condition until single-replica or s3 is used.
- PVCs are create-only in v1. The operator will not patch existing PVCs;
  changing `spec.compiler.storage.cacheSize` post-creation is a no-op.
- Compiler `replicas` is fixed at 1 in v1. Multi-compiler HA requires lease
  coordination across compiler pods (v2 work).
- Secret-rotation does not auto-trigger a rollout in v1. The ConfigMap
  checksum annotation only hashes the rendered config; if you rotate a Secret
  the referenced env var pulls from, bounce the Deployment manually until v2
  extends the annotation.

## Useful values

| Key | Default | Purpose |
|---|---|---|
| `image.repository` | `ghcr.io/bhark/mars-operator` | Operator image |
| `image.tag` | `""` (Chart.appVersion) | Operator image tag |
| `replicaCount` | `1` | Operator replicas; leader election auto-enabled when `> 1` |
| `metrics.port` | `9090` | `/metrics` port |
| `health.port` | `8081` | `/healthz`, `/readyz` port |
| `log.level` | `info` | RUST_LOG-style filter |
| `log.format` | `json` | `json` or `text` |

## See also

- `kubectl explain marsservice.spec` (CRD-driven docs after install).
- `bin/mars-operator/src/crd.rs` for the schema source of truth.
- `bin/mars-operator/src/reconcile.rs` for the reconcile contract.
- `docs/fonts.md` for mounting custom font files into the runtime via
  `spec.runtime.extraVolumes` / `extraVolumeMounts`.
- `docs/symbols.md` for the marker-shape vocabulary and stock preset pack.
