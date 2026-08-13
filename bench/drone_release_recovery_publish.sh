#!/bin/sh
# Promote only the two manifests qualified for the immutable v0.1.0 tag.

set -eu

kind=${1-}
case "$kind" in lb|companion) ;; *) echo "release_recovery_publish=error reason=invalid_publisher" >&2; exit 2 ;; esac
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || {
  echo "release_recovery_publish=error reason=invalid_script_path" >&2
  exit 2
}
sh "$script_dir/drone_release_recovery_guard.sh" "$kind" >/dev/null || {
  echo "release_recovery_publish=error reason=invalid_plan" >&2
  exit 2
}
exec sh "$script_dir/drone_registry_promote.sh" "$kind" v0.1.0 \
  b0e070073d4266018d2f907ff35a7ee88adfdcd4 release_recovery_publish
