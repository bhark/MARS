#!/bin/sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
QUADLET_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/mars"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mars-dev"
MANIFEST_DIR="$DATA_DIR/manifests"

# prereq checks. mikefarah/yq (go) supports `ea`; the python yq does not.
command -v kubectl >/dev/null 2>&1 || { echo "install: kubectl not found in PATH" >&2; exit 1; }
command -v yq >/dev/null 2>&1 || { echo "install: yq not found in PATH" >&2; exit 1; }
if ! yq --version 2>&1 | grep -qE 'mikefarah|version (4|v4)'; then
    echo "install: yq must be mikefarah/yq v4+ (got: $(yq --version 2>&1 | head -1))" >&2
    exit 1
fi

mkdir -p "$QUADLET_DIR" "$MANIFEST_DIR"

# render the local overlay; --load-restrictor=LoadRestrictionsNone lets the
# base configMapGenerator pull dev/config/mars.yaml from outside its dir.
RENDERED="$DATA_DIR/rendered.yaml"
kubectl kustomize \
    --load-restrictor=LoadRestrictionsNone \
    "$ROOT/manifests/overlays/local" > "$RENDERED"

# split by app.kubernetes.io/part-of into per-workload files; each .kube
# unit plays one of these.
for app in config postgis seed compiler runtime viewer; do
    yq ea \
        "select(.metadata.labels.\"app.kubernetes.io/part-of\" == \"$app\")" \
        "$RENDERED" > "$MANIFEST_DIR/$app.yaml"
    if ! [ -s "$MANIFEST_DIR/$app.yaml" ]; then
        echo "install: ERROR: rendered manifest for '$app' is empty" >&2
        exit 1
    fi
done

for f in "$SCRIPT_DIR/quadlets/"*; do
    ln -sfn "$f" "$QUADLET_DIR/$(basename "$f")"
done

systemctl --user daemon-reload

echo "install: quadlets linked to $QUADLET_DIR"
echo "install: manifests rendered to $MANIFEST_DIR"
