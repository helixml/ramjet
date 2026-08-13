#!/bin/sh
# Fail-closed changed-path guard for Drone image publishers.

set -eu

kind=${1-}
case "$kind" in
  rust-deps|lb|companion) ;;
  *)
    echo "publisher_guard=error reason=invalid_publisher" >&2
    exit 2
    ;;
esac

before=${DRONE_COMMIT_BEFORE-}
# DRONE_COMMIT_SHA is documented and is already the immutable release-tag
# source. The observed main #245 environment also exposes the same value as
# DRONE_COMMIT_AFTER, but the guard intentionally does not depend on it.
after=${DRONE_COMMIT_SHA-}
if [ -z "$before" ] || [ -z "$after" ]; then
  echo "publisher_guard=error reason=missing_revision" >&2
  exit 2
fi
if ! git cat-file -e "${before}^{commit}" 2>/dev/null \
  || ! git cat-file -e "${after}^{commit}" 2>/dev/null; then
  echo "publisher_guard=error reason=unavailable_revision" >&2
  exit 2
fi

diff_status=0
git diff --quiet "$before" "$after" -- || diff_status=$?
if [ "$diff_status" -eq 0 ]; then
  echo "publisher_guard=error reason=empty_changeset" >&2
  exit 2
fi
if [ "$diff_status" -ne 1 ]; then
  echo "publisher_guard=error reason=diff_failed" >&2
  exit 2
fi

case "$kind" in
  rust-deps)
    set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
      ':(top)Dockerfile.deps' ':(top).docker/rust-deps-key'
    ;;
  lb)
    set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
      ':(top)Dockerfile' ':(top)compat/**' ':(top)src/**'
    ;;
  companion)
    set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
      ':(top)Dockerfile.companion' ':(top)src/**'
    ;;
esac

owned_status=0
git diff --quiet "$before" "$after" -- "$@" || owned_status=$?
if [ "$owned_status" -eq 1 ]; then
  echo "publisher_guard=publish kind=$kind"
  exit 0
fi
if [ "$owned_status" -ne 0 ]; then
  echo "publisher_guard=error reason=owned_diff_failed" >&2
  exit 2
fi
echo "publisher_guard=skip kind=$kind"
exit 3
