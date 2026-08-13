#!/bin/sh
# Consume the exact revision/tag/target-bound v0.1.0 recovery authority.

set -eu

kind=${1-}
case "$kind" in lb|companion) ;; *) echo "release_recovery_guard=error reason=invalid_publisher" >&2; exit 2 ;; esac

fail() {
  echo "release_recovery_guard=error reason=$1" >&2
  exit 2
}

[ "${DRONE_BUILD_EVENT-}" = promote ] || fail invalid_event
[ "${DRONE_DEPLOY_TO-}" = release-v0.1.0 ] || fail invalid_target
pipeline_sha=${DRONE_COMMIT_SHA-}
[ "${#pipeline_sha}" -eq 40 ] || fail invalid_pipeline_revision
case "$pipeline_sha" in *[!0-9a-fA-F]*) fail invalid_pipeline_revision ;; esac
pipeline_sha=$(printf '%s' "$pipeline_sha" | tr 'A-F' 'a-f')
expected=$(printf '%s\n%s\n%s\n%s' \
  "$pipeline_sha" release-v0.1.0 \
  b0e070073d4266018d2f907ff35a7ee88adfdcd4 v0.1.0)
plan=.drone-release-recovery-plan
marker="$plan/$kind"
[ -d "$plan" ] && [ ! -L "$plan" ] || fail invalid_plan
[ -f "$marker" ] && [ ! -L "$marker" ] || fail invalid_marker
[ "$(cat -- "$marker" 2>/dev/null)" = "$expected" ] || fail invalid_marker
echo "release_recovery_guard=publish kind=$kind"
