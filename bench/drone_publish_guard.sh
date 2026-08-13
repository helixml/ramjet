#!/bin/sh
# Consume the Git-derived publisher plan without requiring .git in this step.

set -eu

kind=${1-}
case "$kind" in
  rust-deps|lb|companion) ;;
  *)
    echo "publisher_guard=error reason=invalid_publisher" >&2
    exit 2
    ;;
esac

is_full_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in
    *[!0-9a-fA-F]*) return 1 ;;
  esac
}

after=${DRONE_COMMIT_SHA-}
if ! is_full_sha "$after"; then
  echo "publisher_guard=error reason=invalid_revision" >&2
  exit 2
fi
after=$(printf '%s' "$after" | tr 'A-F' 'a-f')
plan=.drone-publish-plan
marker="$plan/$kind"
if [ ! -d "$plan" ] || [ -L "$plan" ]; then
  echo "publisher_guard=error reason=invalid_plan" >&2
  exit 2
fi
if [ ! -e "$marker" ] && [ ! -L "$marker" ]; then
  echo "publisher_guard=skip kind=$kind"
  exit 3
fi
if [ ! -f "$marker" ] || [ -L "$marker" ] \
  || [ "$(cat -- "$marker" 2>/dev/null)" != "$after" ]; then
  echo "publisher_guard=error reason=invalid_marker" >&2
  exit 2
fi
echo "publisher_guard=publish kind=$kind"
exit 0
