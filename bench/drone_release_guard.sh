#!/bin/sh
# Consume a validated release plan without relying on Git or Cargo.

set -eu

kind=${1-}
case "$kind" in
  lb|companion) ;;
  *)
    echo "release_guard=error reason=invalid_publisher" >&2
    exit 2
    ;;
esac

[ "${DRONE_BUILD_EVENT-}" = tag ] || {
  echo "release_guard=error reason=invalid_event" >&2
  exit 2
}
tag=${DRONE_TAG-}
sha=${DRONE_COMMIT_SHA-}
case "$sha" in
  *[!0-9a-fA-F]*|'')
    echo "release_guard=error reason=invalid_revision" >&2
    exit 2
    ;;
esac
[ "${#sha}" -eq 40 ] || {
  echo "release_guard=error reason=invalid_revision" >&2
  exit 2
}
sha=$(printf '%s' "$sha" | tr 'A-F' 'a-f')

plan=.drone-release-plan
marker="$plan/$kind"
if [ ! -d "$plan" ] || [ -L "$plan" ]; then
  echo "release_guard=error reason=invalid_plan" >&2
  exit 2
fi
if [ ! -f "$marker" ] || [ -L "$marker" ] \
  || [ "$(cat -- "$marker" 2>/dev/null)" != "$(printf '%s\n%s' "$sha" "$tag")" ]; then
  echo "release_guard=error reason=invalid_marker" >&2
  exit 2
fi
echo "release_guard=publish kind=$kind"
