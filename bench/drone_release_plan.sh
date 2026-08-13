#!/bin/sh
# Validate a release tag and publish a revision-bound plan for Docker steps.

set -eu

plan=.drone-release-plan
temporary="${plan}.tmp.$$"

fail() {
  echo "release_plan=error reason=$1" >&2
  exit 2
}

is_full_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in
    *[!0-9a-fA-F]*) return 1 ;;
  esac
}

[ "${DRONE_BUILD_EVENT-}" = tag ] || fail invalid_event
tag=${DRONE_TAG-}
ref=${DRONE_COMMIT_REF-}
sha=${DRONE_COMMIT_SHA-}
[ -n "$tag" ] && [ -n "$ref" ] && [ -n "$sha" ] || fail missing_identity
is_full_sha "$sha" || fail invalid_revision
sha=$(printf '%s' "$sha" | tr 'A-F' 'a-f')
[ "$ref" = "refs/tags/$tag" ] || fail ref_mismatch

head=$(git rev-parse --verify HEAD^{commit} 2>/dev/null) || fail unavailable_head
head=$(printf '%s' "$head" | tr 'A-F' 'a-f')
[ "$head" = "$sha" ] || fail head_mismatch

package=$(cargo pkgid 2>/dev/null) || fail invalid_manifest
case "$package" in
  *@*) version=${package##*@} ;;
  *) fail invalid_manifest ;;
esac
[ "$tag" = "v$version" ] || fail version_mismatch
[ "${#tag}" -le 128 ] || fail invalid_image_tag
case "$tag" in
  v[0-9]*|v[0-9]*.[0-9]*.[0-9]*) ;;
  *) fail invalid_image_tag ;;
esac
case "$tag" in
  *[!0-9A-Za-z._-]*) fail invalid_image_tag ;;
esac

# Fixed repository-local names only. rm removes a malicious symlink itself,
# never its target, and rename prevents a partially written plan publication.
rm -rf -- "$plan" "$temporary"
umask 077
mkdir -- "$temporary" || fail plan_create_failed
trap 'rm -rf -- "$temporary"' EXIT HUP INT TERM
for kind in lb companion; do
  printf '%s\n%s\n' "$sha" "$tag" >"$temporary/$kind"
done
mv -- "$temporary" "$plan" || fail plan_publish_failed
trap - EXIT HUP INT TERM
echo "release_plan=ready tag=$tag"
