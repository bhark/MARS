# MARS local development (docker compose)

One-command local MARS stack: PostGIS + OSM seed + compiler + runtime + OpenLayers viewer, orchestrated via `docker compose`. Project-scoped (`mars-dev`), hermetic teardown, no host pollution.

## Prerequisites

- Docker 24+ with the Compose v2 plugin (`docker compose version`)
- ~4 GB free disk space

## Quick start

```bash
cd dev
make up           # build images + start stack, blocks on healthchecks
make seed         # one-shot OSM ingest (uses cached extract on rerun)
```

Open http://localhost:5173.

First cold start builds images (~5-8 min); subsequent starts use caches and are fast.

## What gets built

| image | purpose |
|-------|---------|
| `localhost/mars:dev` | MARS runtime + compiler (root `Dockerfile`, cargo-chef) |
| `localhost/mars-seed:dev` | Debian-slim OSM fetcher / loader |
| `localhost/mars-viewer:dev` | nginx + static OpenLayers page |

## Layout

```
dev/
├── docker-compose.yml      # stack definition
├── Makefile                # wrapper targets
├── config/mars.yaml        # canonical MARS config (also used by manifests/base)
├── postgis/init.sql        # postgis bootstrap (extension + schema + slot)
├── seed/                   # osm2pgsql ingester image
└── viewer/                 # static OL viewer image
```

`manifests/base/` is retained as a portable k8s baseline for future cluster deploys; nothing in local dev or e2e references it.

## Make targets

| target | action |
|--------|--------|
| `make build` | Build all three images |
| `make up` | Build, start, and wait for healthchecks |
| `make down` | Stop and remove containers (keeps volumes) |
| `make logs` | Tail logs for all services |
| `make seed` | Run the seed Job once (skips fetch if OSM cache hits) |
| `make reseed` | Drop the postgis volume and seed again |
| `make purge` | Stop, remove containers, volumes, and locally-built images |

## Troubleshooting

**Overpass rate limits:** If seeding fails with HTTP 429, wait a few minutes and `make seed` again. The OSM extract is cached in the `osm-cache` volume.

**Port conflicts:** Runtime binds `8080`, viewer binds `5173`. Edit the `ports:` mappings in `dev/docker-compose.yml` if those are taken.

**Logs:** `docker compose -f dev/docker-compose.yml logs -f <service>` or `make logs`.

**Artifact-store ownership:** the `init-artifact-store` service chowns the shared `artifact-store` volume to uid 65532 (the distroless `nonroot` user) before compiler/runtime start. If you swap the base image to a different non-root UID, edit that one line.

## Reset

```bash
make purge   # full reset: containers, named volumes, locally-built images
```
