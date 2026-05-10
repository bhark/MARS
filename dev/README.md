# MARS Local Development Environment (Quadlet)

One-command local MARS stack: PostGIS + OSM seed + compiler + runtime + OpenLayers viewer, orchestrated via Podman Quadlets.

## Prerequisites

- Linux with systemd
- Podman 5+
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
| `localhost/mars-seed:dev` | Alpine-based OSM fetcher / loader |
| `localhost/mars-viewer:dev` | nginx + static OpenLayers page |

## Make targets

| target | action |
|--------|--------|
| `make dev-build` | Build all three images |
| `make dev-install` | Symlink Quadlets and copy config into `~/.local/share/mars-dev` |
| `make dev-up` | Build, install, and start the full chain |
| `make dev-down` | Stop all services |
| `make dev-logs` | Follow journald logs for all `mars-*` units |
| `make dev-reseed` | Restart the seed container (idempotent; skips fetch if cache exists) |
| `make dev-purge` | Stop, uninstall, and delete all named volumes |

## Troubleshooting

**Overpass rate limits:** If seeding fails with HTTP 429, wait a few minutes and run `make dev-reseed`. The OSM extract is cached in `mars-osm-cache`, so retries are fast.

**Port conflicts:** The runtime binds `8080` and the viewer binds `5173`. Change `PublishPort=` in the Quadlet files if these are taken.

**Logs:** `journalctl --user -u mars-runtime` or `make dev-logs`.

**SELinux (Fedora/RHEL):** The Quadlets use `:Z` on read-only host bind mounts. Named volumes handle their own labelling. If you still see permission denials, check `audit2why`.

## Reset

```bash
make dev-purge   # full reset: images, volumes, config
```
