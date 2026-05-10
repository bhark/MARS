#!/bin/sh
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
QUADLET_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/mars"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mars-dev"

mkdir -p "$QUADLET_DIR" "$DATA_DIR/postgis" "$DATA_DIR/config"

cp -f "$SCRIPT_DIR/postgis/init.sql" "$DATA_DIR/postgis/init.sql"
cp -f "$SCRIPT_DIR/config/mars.yaml" "$DATA_DIR/config/mars.yaml"

for f in "$SCRIPT_DIR/quadlets/"*; do
    ln -sfn "$f" "$QUADLET_DIR/$(basename "$f")"
done

systemctl --user daemon-reload

echo "install: quadlets linked to $QUADLET_DIR"
echo "install: config copied to $DATA_DIR"
