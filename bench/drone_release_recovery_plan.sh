#!/bin/sh
# Authorize the one-time v0.1.0 manifest-copy recovery from an immutable tag.

set -eu

expected_target=release-v0.1.0
expected_tag=v0.1.0
expected_sha=b0e070073d4266018d2f907ff35a7ee88adfdcd4
plan=.drone-release-recovery-plan
temporary="${plan}.tmp.$$"
tag_ref="refs/mini-dynamo-recovery/$expected_tag"

fail() {
  echo "release_recovery_plan=error reason=$1" >&2
  exit 2
}

is_full_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in *[!0-9a-fA-F]*) return 1 ;; esac
}

[ "${DRONE_BUILD_EVENT-}" = promote ] || fail invalid_event
[ "${DRONE_DEPLOY_TO-}" = "$expected_target" ] || fail invalid_target
pipeline_sha=${DRONE_COMMIT_SHA-}
is_full_sha "$pipeline_sha" || fail invalid_pipeline_revision
pipeline_sha=$(printf '%s' "$pipeline_sha" | tr 'A-F' 'a-f')
head=$(git rev-parse --verify HEAD^{commit} 2>/dev/null) || fail unavailable_head
head=$(printf '%s' "$head" | tr 'A-F' 'a-f')
[ "$head" = "$pipeline_sha" ] || fail head_mismatch

git update-ref -d "$tag_ref" >/dev/null 2>&1 || fail tag_ref_cleanup
trap 'git update-ref -d "$tag_ref" >/dev/null 2>&1 || true; rm -rf -- "$temporary"' EXIT HUP INT TERM
GIT_TERMINAL_PROMPT=0 git -c credential.interactive=never fetch --quiet \
  --no-tags --no-recurse-submodules --no-write-fetch-head \
  origin "refs/tags/$expected_tag:$tag_ref" >/dev/null 2>&1 || fail tag_fetch
tag_sha=$(git rev-parse --verify "$tag_ref^{commit}" 2>/dev/null) || fail tag_peel
tag_sha=$(printf '%s' "$tag_sha" | tr 'A-F' 'a-f')
[ "$tag_sha" = "$expected_sha" ] || fail tag_revision_mismatch

manifest=$(git show "$expected_sha:Cargo.toml" 2>/dev/null) || fail tag_manifest
version=$(printf '%s\n' "$manifest" | awk '
  /^\[package\]$/ { package = 1; next }
  /^\[/ && package { exit }
  package && /^version = "[^"]+"$/ { value = $0; sub(/^version = "/, "", value); sub(/"$/, "", value); print value; exit }
')
[ "$version" = 0.1.0 ] || fail version_mismatch

rm -rf -- "$plan" "$temporary"
umask 077
mkdir -- "$temporary" || fail plan_create_failed
for kind in lb companion; do
  printf '%s\n%s\n%s\n%s\n' \
    "$pipeline_sha" "$expected_target" "$expected_sha" "$expected_tag" \
    >"$temporary/$kind"
done
mv -- "$temporary" "$plan" || fail plan_publish_failed
git update-ref -d "$tag_ref" >/dev/null 2>&1 || fail tag_ref_cleanup
trap - EXIT HUP INT TERM
echo "release_recovery_plan=ready tag=$expected_tag"
