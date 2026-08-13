#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 [--check] INFRA_REPOSITORY" >&2
  exit 2
}

check=false
if [[ ${1:-} == "--check" ]]; then
  check=true
  shift
fi
[[ $# -eq 1 ]] || usage

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source_file="$script_dir/docker-compose.yaml"
infra_root=$(git -C "$1" rev-parse --show-toplevel)
target_dir="$infra_root/node06/inference/dspark_0731"
target_file="$target_dir/docker-compose.yaml"

if $check; then
  if cmp -s "$source_file" "$target_file"; then
    echo "compose mirror is current: $target_file"
    exit 0
  fi
  diff -u "$target_file" "$source_file" || true
  echo "compose mirror is stale: run $0 $infra_root" >&2
  exit 1
fi

install -d "$target_dir"
install -m 0644 "$source_file" "$target_file"
echo "updated compose mirror: $target_file"
