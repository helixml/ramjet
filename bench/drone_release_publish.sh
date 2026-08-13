#!/bin/sh
# Validate and retag one already-qualified immutable image without rebuilding it.

set -eu

kind=${1-}
case "$kind" in lb|companion) ;; *) echo "release_publish=error reason=invalid_publisher" >&2; exit 2 ;; esac

fail() {
  echo "release_publish=error reason=$1" >&2
  exit 2
}

[ "${DRONE_BUILD_EVENT-}" = tag ] || fail invalid_event
tag=${DRONE_TAG-}
sha=${DRONE_COMMIT_SHA-}
[ "${#sha}" -eq 40 ] || fail invalid_revision
case "$sha" in *[!0-9a-fA-F]*) fail invalid_revision ;; esac
sha=$(printf '%s' "$sha" | tr 'A-F' 'a-f')
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || fail invalid_script_path

sh "$script_dir/drone_release_guard.sh" "$kind" >/dev/null || fail invalid_plan
exec sh "$script_dir/drone_registry_promote.sh" "$kind" "$tag" "$sha"
