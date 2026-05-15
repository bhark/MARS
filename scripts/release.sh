#!/usr/bin/env bash
set -euo pipefail

# scripts/release.sh - cut a MARS release.
#
# pushes a vX.Y.Z tag from main; .github/workflows/release.yml then patches
# the workspace + chart versions in the runner from the tag and publishes
# binary tarball, multi-arch container images, and the OCI helm chart.
# version files on main stay at the 0.0.0-dev placeholder; never bump them
# in a PR.
#
# usage: scripts/release.sh <patch|minor|major|vX.Y.Z[-rc.N]> [--yes] [--watch] [--allow-red-ci] [--dry-run]

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

YES=0
WATCH=0
ALLOW_RED_CI=0
DRY_RUN=0
BUMP=""

usage() {
  cat <<EOF
usage: scripts/release.sh <patch|minor|major|vX.Y.Z[-rc.N]> [flags]

flags:
  --yes             skip the confirm prompt
  --watch           gh run watch the workflow after pushing the tag
  --allow-red-ci    skip the gate that requires the latest CI run on main to be green
  --dry-run         run all preflight checks, print the tag command, do not push
  -h, --help        show this message

examples:
  scripts/release.sh patch          # latest tag v0.1.4 -> v0.1.5
  scripts/release.sh minor          # latest tag v0.1.4 -> v0.2.0
  scripts/release.sh v0.2.0-rc.1    # explicit (use for prereleases)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --yes) YES=1; shift ;;
    --watch) WATCH=1; shift ;;
    --allow-red-ci) ALLOW_RED_CI=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
    *)
      if [[ -n "${BUMP}" ]]; then
        echo "unexpected positional arg: $1" >&2; usage >&2; exit 2
      fi
      BUMP="$1"; shift ;;
  esac
done

if [[ -z "${BUMP}" ]]; then
  usage >&2; exit 2
fi

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing required command: $1" >&2; exit 2; }; }

need git
need gh

cd "${ROOT}"

# preflight: clean tree
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "working tree not clean. commit or stash first." >&2
  git status --short >&2
  exit 1
fi

# preflight: on main
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "${BRANCH}" != "main" ]]; then
  echo "not on main (currently on ${BRANCH}). releases must be cut from main." >&2
  exit 1
fi

# preflight: up to date with origin
log "fetching origin/main"
git fetch --quiet origin main
LOCAL="$(git rev-parse HEAD)"
REMOTE="$(git rev-parse origin/main)"
if [[ "${LOCAL}" != "${REMOTE}" ]]; then
  echo "local main is not at origin/main (local=${LOCAL:0:12} remote=${REMOTE:0:12})." >&2
  echo "rebase / pull first." >&2
  exit 1
fi

# preflight: latest CI green
if [[ "${ALLOW_RED_CI}" != "1" ]]; then
  CI_STATE="$(gh run list --branch main --workflow CI --limit 1 --json conclusion --jq '.[0].conclusion // "missing"')"
  if [[ "${CI_STATE}" != "success" ]]; then
    echo "latest CI run on main is '${CI_STATE}', not 'success'." >&2
    echo "fix CI first, or pass --allow-red-ci to override." >&2
    exit 1
  fi
fi

# compute new tag
LAST="$(git tag --list 'v*' --sort=-v:refname | head -1)"
[[ -n "${LAST}" ]] || LAST="v0.0.0"

semver_re='^v([0-9]+)\.([0-9]+)\.([0-9]+)$'
explicit_re='^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

case "${BUMP}" in
  patch|minor|major)
    if ! [[ "${LAST}" =~ ${semver_re} ]]; then
      echo "latest tag '${LAST}' is not a plain vX.Y.Z; pass an explicit version (e.g. v0.2.0)." >&2
      exit 1
    fi
    MAJ="${BASH_REMATCH[1]}"
    MIN="${BASH_REMATCH[2]}"
    PAT="${BASH_REMATCH[3]}"
    case "${BUMP}" in
      patch) PAT=$((PAT + 1)) ;;
      minor) MIN=$((MIN + 1)); PAT=0 ;;
      major) MAJ=$((MAJ + 1)); MIN=0; PAT=0 ;;
    esac
    NEW="v${MAJ}.${MIN}.${PAT}"
    ;;
  v*)
    if ! [[ "${BUMP}" =~ ${explicit_re} ]]; then
      echo "explicit tag '${BUMP}' is not vX.Y.Z[-suffix]." >&2
      exit 1
    fi
    NEW="${BUMP}"
    ;;
  *)
    echo "unknown bump '${BUMP}'." >&2
    usage >&2; exit 2 ;;
esac

if git rev-parse --verify --quiet "${NEW}" >/dev/null; then
  echo "tag '${NEW}' already exists." >&2
  exit 1
fi

# confirm
SUBJECT="$(git log -1 --pretty=%s HEAD)"
REMOTE_URL="$(git remote get-url origin)"
cat <<EOF

about to:
  tag    : ${NEW}
  commit : ${LOCAL:0:12}  ${SUBJECT}
  remote : ${REMOTE_URL}

this triggers .github/workflows/release.yml on push.

EOF

if [[ "${DRY_RUN}" == "1" ]]; then
  log "dry-run: would run 'git tag -a ${NEW} -m ${NEW} && git push origin ${NEW}'"
  exit 0
fi

if [[ "${YES}" != "1" ]]; then
  printf "continue? [y/N] "
  read -r REPLY
  case "${REPLY}" in
    y|Y|yes|YES) ;;
    *) log "aborted by user"; exit 1 ;;
  esac
fi

log "tagging ${NEW}"
git tag -a "${NEW}" -m "${NEW}"
log "pushing ${NEW}"
git push origin "${NEW}"

if [[ "${WATCH}" == "1" ]]; then
  # the run is keyed on the tag-push event; give gh a moment to register it.
  sleep 5
  log "watching release workflow"
  gh run watch --exit-status \
    "$(gh run list --workflow release --limit 1 --json databaseId --jq '.[0].databaseId')"
fi

log "ok: ${NEW} pushed"
