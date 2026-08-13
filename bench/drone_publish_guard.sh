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

is_full_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in
    *[!0-9a-fA-F]*) return 1 ;;
  esac
}

# Validate before invoking Git so an attacker-controlled revision can never be
# interpreted as an option or refspec.
if ! is_full_sha "$before" || ! is_full_sha "$after"; then
  echo "publisher_guard=error reason=invalid_revision" >&2
  exit 2
fi
if ! git cat-file -e "${after}^{commit}" 2>/dev/null; then
  echo "publisher_guard=error reason=unavailable_after" >&2
  exit 2
fi
if ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
  # Drone's clone is shallow. Fetch only the already-validated exact predecessor
  # commit, without tags, history, submodules, or a persistent FETCH_HEAD.
  if ! GIT_TERMINAL_PROMPT=0 git -c credential.interactive=never fetch --quiet \
    --no-tags --no-recurse-submodules --no-write-fetch-head --depth=1 \
    origin "$before" >/dev/null 2>&1; then
    echo "publisher_guard=error reason=fetch_failed" >&2
    exit 2
  fi
  if ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
    echo "publisher_guard=error reason=unavailable_before" >&2
    exit 2
  fi
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
