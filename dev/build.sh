#!/bin/sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "build: mars runtime image"
podman build -t localhost/mars:dev -f "$ROOT/Dockerfile" "$ROOT"

echo "build: mars-seed image"
podman build -t localhost/mars-seed:dev -f "$ROOT/dev/seed/Containerfile" "$ROOT/dev/seed"

echo "build: mars-viewer image"
podman build -t localhost/mars-viewer:dev -f "$ROOT/dev/viewer/Containerfile" "$ROOT/dev/viewer"

echo "build: done"
