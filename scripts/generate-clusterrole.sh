#!/usr/bin/env bash
# generate-clusterrole.sh - regenerate the operator ClusterRole chart template
# from the operator code (single source of truth at
# bin/mars-operator/src/clusterrole.rs) and write it to
# charts/mars-operator/templates/clusterrole.yaml. CI compares the regenerated
# output against the committed file to catch drift, mirroring generate-crd.sh.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${repo_root}/charts/mars-operator/templates/clusterrole.yaml"

cd "${repo_root}"
cargo run --quiet -p mars-operator -- print-clusterrole > "${out}"

echo "wrote ${out}"
