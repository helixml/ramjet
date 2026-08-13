#!/usr/bin/env bash
set -euo pipefail

runtime_a=${SNAPSHOT_RUNTIME_DIR_A:-}
runtime_b=${SNAPSHOT_RUNTIME_DIR_B:-}
secret_a=${SNAPSHOT_SESSION_SECRET_FILE_A:-}
secret_b=${SNAPSHOT_SESSION_SECRET_FILE_B:-}

fail() {
  echo "snapshot companion host preflight failed: $1" >&2
  exit 1
}

require_normalized_unlinked_path() {
  local path=$1
  local kind=$2
  local current=/
  local -a components

  [[ $path == /* ]] || fail "$kind path must be absolute"
  [[ $(readlink -f -- "$path") == "$path" ]] || fail "$kind path is not normalized"
  IFS=/ read -r -a components <<<"${path#/}"
  for component in "${components[@]}"; do
    [[ -n $component ]] || continue
    current=${current%/}/$component
    [[ ! -L $current ]] || fail "$kind path contains a symlink"
  done
}

validate_runtime() {
  local engine=$1
  local path=$2
  local owner_uid=$3

  [[ -d $path && ! -L $path ]] || fail "$engine runtime directory is missing or linked"
  require_normalized_unlinked_path "$path" "$engine runtime"
  [[ $(stat -f -c '%T' -- "$path") == tmpfs ]] || fail "$engine runtime directory is not on tmpfs"
  [[ $(stat -c '%u:%g:%a' -- "$path") == "$owner_uid:12000:750" ]] || \
    fail "$engine runtime directory must be UID $owner_uid, GID 12000, mode 0750"
}

validate_secret() {
  local engine=$1
  local path=$2

  [[ -f $path && ! -L $path ]] || fail "$engine secret is missing, linked, or not regular"
  require_normalized_unlinked_path "$path" "$engine secret"
  [[ $(stat -c '%u:%g:%a:%h:%s' -- "$path") == 0:12000:440:1:32 ]] || \
    fail "$engine secret must be UID 0, GID 12000, mode 0440, one link, exactly 32 raw bytes"
}

[[ -n $runtime_a ]] || fail "SNAPSHOT_RUNTIME_DIR_A is required"
[[ -n $runtime_b ]] || fail "SNAPSHOT_RUNTIME_DIR_B is required"
[[ -n $secret_a ]] || fail "SNAPSHOT_SESSION_SECRET_FILE_A is required"
[[ -n $secret_b ]] || fail "SNAPSHOT_SESSION_SECRET_FILE_B is required"

[[ $runtime_a != "$runtime_b" ]] || fail "per-engine runtime directories must differ"
[[ $secret_a != "$secret_b" ]] || fail "per-engine secret files must differ"

validate_runtime engine-a "$runtime_a" 12001
validate_runtime engine-b "$runtime_b" 12003
validate_secret engine-a "$secret_a"
validate_secret engine-b "$secret_b"

[[ $(stat -c '%d:%i' -- "$runtime_a") != "$(stat -c '%d:%i' -- "$runtime_b")" ]] || \
  fail "per-engine runtime directories resolve to the same inode"
[[ $(stat -c '%d:%i' -- "$secret_a") != "$(stat -c '%d:%i' -- "$secret_b")" ]] || \
  fail "per-engine secrets resolve to the same inode"

echo "snapshot companion host preflight passed: two isolated authority domains"
