# MARS Local Development Environment (Quadlet + Kubernetes manifests)

One-command local MARS stack: PostGIS + OSM seed + compiler + runtime + OpenLayers viewer, orchestrated via Podman Quadlet `.kube` units that play Kubernetes YAML manifests.

The same manifests under `manifests/` drive both local dev (`overlays/local`) and any future cluster deploy. Adding a new env (prod, staging, e2e) means writing a new overlay, not a new set of unit files.

## Prerequisites

- Linux with systemd
- Podman 5+
- `kubectl` 1.27+ (built-in kustomize)
- `yq` 4+ (mikefarah/yq, package `go-yq` on Arch / `yq` on most distros - **not** the python `yq`)
- `loginctl enable-linger $USER` if you want the stack to survive logout
- ~4 GB free disk space

## Quick start

```bash
cd dev
make dev-up
```

Then open http://localhost:5173.

First cold start builds images (~5-8 min) and fetches OSM data (~1-3 min); subsequent starts use caches and are fast.

## What gets built

| image | purpose |
|-------|---------|
| `localhost/mars:dev` | MARS runtime + compiler (from root `Dockerfile` with cargo-chef) |
| `localhost/mars-seed:dev` | Debian-slim OSM fetcher / loader |
| `localhost/mars-viewer:dev` | nginx + static OpenLayers page |

## Manifest layout

```
manifests/
├── base/                    # env-agnostic: compiler/runtime Deployments,
│                            # runtime Service, mars-config ConfigMap (no
│                            # artifact-store backend pinned).
└── overlays/local/          # local dev: postgis Deployment+Service+Secret,
                             # seed Job, viewer Pod, PVCs, mars-artifact-store
                             # wired RW into compiler / RO into runtime.
```

`dev-install` renders `overlays/local` with `kubectl kustomize`, splits the result by `app.kubernetes.io/part-of` label into one YAML file per workload, and symlinks the `.kube` units that play them.

## Make targets

| target | action |
|--------|--------|
| `make dev-build` | Build all three images |
| `make dev-install` | Render manifests, split per workload, symlink Quadlets |
| `make dev-up` | Build, install, and start the full chain |
| `make dev-down` | Stop all services |
| `make dev-logs` | Follow journald logs for all `mars-*` units |
| `make dev-reseed` | Restart the seed Job (idempotent; skips fetch if cache exists) |
| `make dev-purge` | Stop, uninstall, and delete all named volumes |

## Troubleshooting

**Overpass rate limits:** If seeding fails with HTTP 429, wait a few minutes and run `make dev-reseed`. The OSM extract is cached in `mars-osm-cache`, so retries are fast.

**Port conflicts:** The runtime binds `8080` and the viewer binds `5173` via `hostPort` on their respective Pods. Change the `hostPort` values in `manifests/overlays/local/runtime-volume-patch.yaml` and `manifests/overlays/local/viewer.yaml` if these are taken, then `make dev-install && make dev-up`.

**Logs:** `journalctl --user -u mars-runtime` or `make dev-logs`. Each `.kube` unit logs both `kube play` output and the container logs streamed by Podman.

**SELinux (Fedora/RHEL):** ConfigMaps and Secrets land as named volumes inside the pod's mount namespace - no host bind mounts to relabel. If you still see permission denials, check `audit2why`.

**Shared UID for the artifact store:** Both the compiler (rw) and runtime (ro) containers run as UID 65532 (the distroless `nonroot` user). This is intentional - the `mars-artifact-store` PVC (a podman volume) is created under that UID on first start, and the runtime can read what the compiler writes without further wiring. If you swap the base image to something with a different non-root UID, expect to chown the volume.

## Reset

```bash
make dev-purge   # full reset: quadlets, manifests, podman volumes
```
