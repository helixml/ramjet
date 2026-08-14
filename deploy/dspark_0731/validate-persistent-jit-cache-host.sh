#!/usr/bin/env bash
set -euo pipefail

fingerprint=vllme2666d9a65-b12x7cecbb2c48-136ce64f2c43f0f8
base=/prod/mini-dynamo/jit-cache/${fingerprint}
paths=("${base}/engine-a" "${base}/engine-b")

parents=(/prod /prod/mini-dynamo /prod/mini-dynamo/jit-cache "${base}")
for parent in "${parents[@]}"; do
  if [[ ! -d ${parent} || -L ${parent} || $(readlink -e -- "${parent}") != "${parent}" ]]; then
    echo "persistent JIT-cache host validation failed: cache parent is unsafe" >&2
    exit 1
  fi
  owner=$(stat -c '%u' -- "${parent}")
  mode=$(stat -c '%a' -- "${parent}")
  if [[ ${owner} != 0 ]] || (( (8#${mode} & 8#022) != 0 )); then
    echo "persistent JIT-cache host validation failed: cache parent is writable" >&2
    exit 1
  fi
done

for path in "${paths[@]}"; do
  if [[ ! -d ${path} || -L ${path} ]]; then
    echo "persistent JIT-cache host validation failed: cache directory is unavailable" >&2
    exit 1
  fi
  if [[ $(readlink -e -- "${path}") != "${path}" ]]; then
    echo "persistent JIT-cache host validation failed: cache path contains a link" >&2
    exit 1
  fi
  if [[ $(stat -c '%u:%g:%a' -- "${path}") != 0:0:700 ]]; then
    echo "persistent JIT-cache host validation failed: cache ownership or mode changed" >&2
    exit 1
  fi
  filesystem=$(findmnt -n -o FSTYPE -T "${path}")
  case "${filesystem}" in
    tmpfs|ramfs|overlay)
      echo "persistent JIT-cache host validation failed: cache is not disk-backed" >&2
      exit 1
      ;;
  esac
done

first_inode=$(stat -c '%d:%i' -- "${paths[0]}")
second_inode=$(stat -c '%d:%i' -- "${paths[1]}")
if [[ ${first_inode} == "${second_inode}" ]]; then
  echo "persistent JIT-cache host validation failed: engine writers share a directory" >&2
  exit 1
fi

# Keep enough headroom for two independently warming TP4 cache trees. This is
# an admission check, not a quota; live disk-pressure monitoring remains
# mandatory during the one-engine trial.
available_kib=$(df -Pk -- "${base}" | awk 'NR == 2 {print $4}')
if [[ ! ${available_kib} =~ ^[0-9]+$ || ${available_kib} -lt 16777216 ]]; then
  echo "persistent JIT-cache host validation failed: less than 16 GiB is free" >&2
  exit 1
fi

echo "persistent JIT-cache host validation passed: isolated disk-backed writers"
