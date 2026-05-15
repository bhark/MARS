#!/usr/bin/env bash
# shared helpers for the fixture fetchers (tests/e2e and tests/parity).
# sourced, not executed. callers declare:
#   DEST          absolute path where the dump lives
#   MANIFEST      absolute path to manifest.sha256 (header comments + sha line)
#   FILE_KEY      the filename as it appears in the manifest sha line
#   ENV_OVERRIDE  name of an env var that, if set, overrides the manifest url
# then call fetch_fixture::ensure.
#
# manifest format (one canonical url + one sha line, comments allowed):
#   # source: https://github.com/bhark/MARS/releases/download/<tag>/<file>
#   # tag: <tag>
#   <sha256>  <FILE_KEY>

# shellcheck shell=bash

fetch_fixture::_need() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'fetch-fixture: missing required command: %s\n' "$1" >&2
    return 2
  }
}

# read sha line for a given file key out of a manifest, ignoring comments.
fetch_fixture::_expected_sha() {
  local manifest="$1" file_key="$2"
  [[ -f "${manifest}" ]] || return 0
  awk -v k="${file_key}" '
    /^[[:space:]]*#/ { next }
    $2 == k { print $1; exit }
  ' "${manifest}"
}

# read the canonical url from the manifest header comment.
fetch_fixture::_manifest_url() {
  local manifest="$1"
  [[ -f "${manifest}" ]] || return 0
  awk '
    /^[[:space:]]*#[[:space:]]*source:[[:space:]]*/ {
      sub(/^[[:space:]]*#[[:space:]]*source:[[:space:]]*/, "")
      print
      exit
    }
  ' "${manifest}"
}

fetch_fixture::_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

# download url to dest atomically (via .partial), with retries.
fetch_fixture::download() {
  local url="$1" dest="$2"
  printf 'fetch-fixture: fetching %s\n' "${url}"
  curl -fL --retry 3 --retry-delay 2 -o "${dest}.partial" "${url}"
  mv "${dest}.partial" "${dest}"
}

# verify dest matches the manifest entry. returns 0 on match, 1 on mismatch,
# 2 if the manifest doesn't declare a sha for this file (treat as "no expectation").
fetch_fixture::verify() {
  local dest="$1" manifest="$2" file_key="$3"
  local expected actual
  expected="$(fetch_fixture::_expected_sha "${manifest}" "${file_key}")"
  if [[ -z "${expected}" ]]; then
    return 2
  fi
  actual="$(fetch_fixture::_sha256 "${dest}")"
  if [[ "${expected}" != "${actual}" ]]; then
    printf 'fetch-fixture: sha256 mismatch for %s (expected %s, got %s)\n' \
      "${file_key}" "${expected}" "${actual}" >&2
    return 1
  fi
  return 0
}

# public entry. idempotent: skips work when dest is present and matches.
fetch_fixture::ensure() {
  local dest="$1" manifest="$2" file_key="$3" env_override="$4"

  fetch_fixture::_need curl || return $?
  fetch_fixture::_need sha256sum || return $?
  fetch_fixture::_need awk || return $?

  mkdir -p "$(dirname "${dest}")"

  if [[ -f "${dest}" ]]; then
    local verify_rc=0
    fetch_fixture::verify "${dest}" "${manifest}" "${file_key}" || verify_rc=$?
    case ${verify_rc} in
      0)
        printf 'fetch-fixture: present and verified: %s\n' "${dest}"
        return 0
        ;;
      2)
        # manifest has no sha pin for this file. treat the local file as
        # authoritative but warn so the maintainer notices the gap.
        printf 'fetch-fixture: manifest has no sha for %s; using local %s as-is\n' \
          "${file_key}" "${dest}" >&2
        return 0
        ;;
    esac
    # mismatch: fall through to re-download.
    printf 'fetch-fixture: local file mismatched manifest; re-downloading\n' >&2
  fi

  local url="${!env_override:-}"
  if [[ -z "${url}" ]]; then
    url="$(fetch_fixture::_manifest_url "${manifest}")"
  fi
  if [[ -z "${url}" ]]; then
    cat >&2 <<EOF
fetch-fixture: no source url configured.
neither ${env_override} is set nor does ${manifest} carry a '# source:' line.
populate the manifest (cut a GitHub Release first; see scripts/release-fixtures.sh)
or place the dump manually at:
  ${dest}
EOF
    return 2
  fi

  fetch_fixture::download "${url}" "${dest}"

  local verify_rc=0
  fetch_fixture::verify "${dest}" "${manifest}" "${file_key}" || verify_rc=$?
  case ${verify_rc} in
    0)
      printf 'fetch-fixture: ready: %s\n' "${dest}"
      return 0
      ;;
    2)
      # downloaded but manifest has no pin. accept, warn loudly.
      printf 'fetch-fixture: downloaded %s but manifest has no sha to verify against\n' \
        "${file_key}" >&2
      return 0
      ;;
    *)
      # mismatch on freshly downloaded asset: hard fail. don't leave a
      # poisoned file in place.
      rm -f "${dest}"
      return 1
      ;;
  esac
}
