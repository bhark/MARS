#!/bin/sh
set -eu

# data dirs to chown on startup; override via MARS_DATA_DIRS (colon-separated).
# defaults cover the union of compiler+runtime mount points referenced by the
# in-tree manifests (manifests/base/* and overlays/{local,e2e}).
: "${MARS_DATA_DIRS:=/cache:/work:/var/lib/mars/store}"
: "${MARS_RUN_UID:=65532}"
: "${MARS_RUN_GID:=65532}"

# only bootstrap when started as root. supports environments that already
# drop privileges externally (kubelet w/ fsGroup, podman --user=...).
if [ "$(id -u)" = "0" ]; then
  IFS=:
  for dir in $MARS_DATA_DIRS; do
    [ -d "$dir" ] || continue
    # tolerate read-only mounts (runtime mounts artifact-store ro); chown
    # there is rejected by the kernel - swallow it.
    chown -R "$MARS_RUN_UID:$MARS_RUN_GID" "$dir" 2>/dev/null || true
  done
  unset IFS
  exec setpriv --reuid="$MARS_RUN_UID" --regid="$MARS_RUN_GID" \
       --clear-groups --inh-caps=-all -- /usr/local/bin/mars "$@"
fi

exec /usr/local/bin/mars "$@"
