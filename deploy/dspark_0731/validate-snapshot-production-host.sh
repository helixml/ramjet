#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "snapshot production host preflight failed: $1" >&2
  exit 1
}

mode=${1:-full}
[[ $mode == pre-provision || $mode == full ]] || \
  fail "usage: validate-snapshot-production-host.sh [pre-provision|full]"

required() {
  local name=$1
  local value=${!name:-}
  [[ -n $value ]] || fail "$name is required"
  printf '%s' "$value"
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

validate_directory() {
  local engine=$1
  local kind=$2
  local path=$3
  local identity=$4

  [[ -d $path && ! -L $path ]] || fail "$engine $kind directory is missing or linked"
  require_normalized_unlinked_path "$path" "$engine $kind"
  [[ $(stat -f -c '%T' -- "$path") == tmpfs ]] || fail "$engine $kind directory is not on tmpfs"
  [[ $(stat -c '%u:%g:%a' -- "$path") == "$identity" ]] || \
    fail "$engine $kind directory identity/mode is not $identity"
}

validate_secret() {
  local engine=$1
  local kind=$2
  local path=$3

  [[ -f $path && ! -L $path ]] || fail "$engine $kind is missing, linked, or not regular"
  require_normalized_unlinked_path "$path" "$engine $kind"
  [[ $(stat -c '%u:%g:%a:%h:%s' -- "$path") == 0:12000:440:1:32 ]] || \
    fail "$engine $kind must be UID 0, GID 12000, mode 0440, one link, exactly 32 raw bytes"
}

validate_attestation() {
  local engine=$1
  local directory=$2
  local path=$directory/engine.json

  validate_directory "$engine" attestation "$directory" 0:12000:2750
  [[ $mode == full ]] || return 0
  [[ -f $path && ! -L $path ]] || fail "$engine attestation is missing or unsafe"
  [[ $(stat -c '%u:%g:%a:%h' -- "$path") == 0:12000:440:1 ]] || \
    fail "$engine attestation must be UID 0, GID 12000, mode 0440, one link"
  [[ $(stat -c '%s' -- "$path") -gt 0 ]] || fail "$engine attestation is empty"
}

validate_metadata() {
  local engine=$1
  local path=$2

  [[ -f $path && ! -L $path ]] || fail "$engine metadata is missing or unsafe"
  require_normalized_unlinked_path "$path" "$engine metadata"
  [[ $(stat -c '%u:%g:%a:%h' -- "$path") == 0:0:600:1 ]] || \
    fail "$engine metadata must be root-owned, mode 0600, one link"
  local size
  size=$(stat -c '%s' -- "$path")
  (( size > 0 && size <= 65536 )) || fail "$engine metadata size is outside the provisioner bound"
}

declare -a all_paths=()
for suffix in A B; do
  runtime=$(required SNAPSHOT_RUNTIME_DIR_${suffix})
  metrics=$(required SNAPSHOT_METRICS_DIR_${suffix})
  session=$(required SNAPSHOT_SESSION_SECRET_FILE_${suffix})
  digest=$(required SNAPSHOT_DIGEST_SECRET_FILE_${suffix})
  attestation=$(required SNAPSHOT_ATTESTATION_DIR_${suffix})
  metadata=$(required SNAPSHOT_ENGINE_METADATA_FILE_${suffix})

  if [[ $suffix == A ]]; then
    engine=engine-a
    companion_uid=12001
    metrics_gid=12004
  else
    engine=engine-b
    companion_uid=12003
    metrics_gid=12005
  fi

  validate_directory "$engine" snapshot "$runtime" "$companion_uid:12000:2750"
  validate_directory "$engine" metrics "$metrics" "$companion_uid:$metrics_gid:2750"
  validate_secret "$engine" session-secret "$session"
  validate_secret "$engine" digest-secret "$digest"
  validate_attestation "$engine" "$attestation"
  validate_metadata "$engine" "$metadata"
  [[ $(stat -c '%d:%i' -- "$runtime") != "$(stat -c '%d:%i' -- "$metrics")" ]] || \
    fail "$engine snapshot and metrics directories resolve to one inode"
  all_paths+=("$runtime" "$metrics" "$session" "$digest" "$attestation" "$metadata")
done

for ((left = 0; left < ${#all_paths[@]}; left++)); do
  for ((right = left + 1; right < ${#all_paths[@]}; right++)); do
    [[ ${all_paths[left]} != "${all_paths[right]}" ]] || fail "authority paths must all differ"
    [[ $(stat -c '%d:%i' -- "${all_paths[left]}") != "$(stat -c '%d:%i' -- "${all_paths[right]}")" ]] || \
      fail "authority paths resolve to one inode"
  done
done

echo "snapshot production host preflight passed ($mode): two isolated shadow domains"
