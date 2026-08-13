#!/usr/bin/env bash
set -euo pipefail

runtime_dir=${SNAPSHOT_RUNTIME_DIR:-}
secret_file=${SNAPSHOT_SESSION_SECRET_FILE:-}

fail() {
  echo "snapshot companion host preflight failed: $1" >&2
  exit 1
}

[[ -n $runtime_dir ]] || fail "SNAPSHOT_RUNTIME_DIR is required"
[[ -n $secret_file ]] || fail "SNAPSHOT_SESSION_SECRET_FILE is required"
[[ $runtime_dir == /* && $secret_file == /* ]] || fail "paths must be absolute"
[[ -d $runtime_dir && ! -L $runtime_dir ]] || fail "runtime directory is missing or linked"
[[ -f $secret_file && ! -L $secret_file ]] || fail "secret is missing, linked, or not regular"
[[ $(readlink -f -- "$runtime_dir") == "$runtime_dir" ]] || fail "runtime path is not normalized"
[[ $(readlink -f -- "$secret_file") == "$secret_file" ]] || fail "secret path is not normalized"

runtime_type=$(stat -f -c '%T' -- "$runtime_dir")
[[ $runtime_type == tmpfs ]] || fail "runtime directory is not on tmpfs"
[[ $(stat -c '%u:%g:%a' -- "$runtime_dir") == 12001:12000:750 ]] || \
  fail "runtime directory must be UID 12001, GID 12000, mode 0750"
[[ $(stat -c '%u:%g:%a:%h:%s' -- "$secret_file") == 0:12000:440:1:32 ]] || \
  fail "secret must be UID 0, GID 12000, mode 0440, one link, exactly 32 raw bytes"

current=/
IFS=/ read -r -a components <<<"${runtime_dir#/}"
for component in "${components[@]}"; do
  [[ -n $component ]] || continue
  current=${current%/}/$component
  [[ ! -L $current ]] || fail "runtime path contains a symlink"
done
current=/
IFS=/ read -r -a components <<<"${secret_file#/}"
for component in "${components[@]}"; do
  [[ -n $component ]] || continue
  current=${current%/}/$component
  [[ ! -L $current ]] || fail "secret path contains a symlink"
done

echo "snapshot companion host preflight passed"
