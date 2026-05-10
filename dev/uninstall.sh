#!/bin/sh
set -eu

QUADLET_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/mars"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mars-dev"

if [ -d "$QUADLET_DIR" ]; then
    for f in "$QUADLET_DIR/"*; do
        if [ -L "$f" ]; then
            rm -f "$f"
        fi
    done
    # remove dir only if empty
    rmdir "$QUADLET_DIR" 2>/dev/null || true
fi

rm -rf "$DATA_DIR"

systemctl --user daemon-reload

echo "uninstall: removed quadlets and data"
