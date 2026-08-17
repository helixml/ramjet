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
infra_root=$(git -C "$1" rev-parse --show-toplevel)
target_dir="$infra_root/node06/inference/qwen38_27b"
source_files=("$script_dir"/*.yaml)

if $check; then
  stale=false
  for source_file in "${source_files[@]}"; do
    target_file="$target_dir/$(basename -- "$source_file")"
    if [[ ! -f $target_file ]]; then
      echo "compose mirror is missing: $target_file" >&2
      stale=true
      continue
    fi
    if ! cmp -s "$source_file" "$target_file"; then
      diff -u "$target_file" "$source_file" || true
      stale=true
    fi
  done
  if $stale; then
    echo "Qwen compose mirror is stale: run $0 $infra_root" >&2
    exit 1
  fi
  echo "Qwen compose mirror is current: $target_dir"
  exit 0
fi

install -d "$target_dir"
for source_file in "${source_files[@]}"; do
  install -m 0644 "$source_file" "$target_dir/$(basename -- "$source_file")"
done
echo "updated Qwen compose mirror: $target_dir"
