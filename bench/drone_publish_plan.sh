#!/bin/sh
# Build a fail-closed image-publisher plan while the Drone clone still has Git.

set -eu

if [ "${DRONE_BUILD_EVENT-}" != push ]; then
  echo "publisher_plan=skip reason=non_push"
  exit 0
fi

before=${DRONE_COMMIT_BEFORE-}
after=${DRONE_COMMIT_SHA-}
plan=.drone-publish-plan
temporary="${plan}.tmp.$$"

is_full_sha() {
  [ "${#1}" -eq 40 ] || return 1
  case "$1" in
    *[!0-9a-fA-F]*) return 1 ;;
  esac
}

fail() {
  echo "publisher_plan=error reason=$1" >&2
  exit 2
}

[ -n "$before" ] && [ -n "$after" ] || fail missing_revision
is_full_sha "$before" && is_full_sha "$after" || fail invalid_revision

head=$(git rev-parse --verify HEAD^{commit} 2>/dev/null) || fail unavailable_head
head=$(printf '%s' "$head" | tr 'A-F' 'a-f')
after=$(printf '%s' "$after" | tr 'A-F' 'a-f')
before=$(printf '%s' "$before" | tr 'A-F' 'a-f')
[ "$head" = "$after" ] || fail head_mismatch

if ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
  if ! GIT_TERMINAL_PROMPT=0 git -c credential.interactive=never fetch --quiet \
    --no-tags --no-recurse-submodules --no-write-fetch-head --depth=1 \
    origin "$before" >/dev/null 2>&1; then
    fail fetch_failed
  fi
  git cat-file -e "${before}^{commit}" 2>/dev/null || fail unavailable_before
fi

diff_status=0
git diff --quiet "$before" "$after" -- || diff_status=$?
[ "$diff_status" -ne 0 ] || fail empty_changeset
[ "$diff_status" -eq 1 ] || fail diff_failed

# These are fixed repository-local names. Removing a symlink removes the link,
# never its target. A private temporary directory prevents partial publication.
rm -rf -- "$plan" "$temporary"
umask 077
mkdir -- "$temporary" || fail plan_create_failed
trap 'rm -rf -- "$temporary"' EXIT HUP INT TERM

for kind in rust-deps release-tools lb companion; do
  case "$kind" in
    rust-deps)
      set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
        ':(top)Dockerfile.deps' ':(top).docker/rust-deps-key'
      ;;
    release-tools)
      set -- ':(top)Dockerfile.release-tools' \
        ':(top).docker/release-tools-key'
      ;;
    lb)
      set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
        ':(top)Dockerfile' ':(top)compat/**' ':(top)src/**' ':(top)web/**'
      ;;
    companion)
      set -- ':(top)Cargo.toml' ':(top)Cargo.lock' ':(top)rust-toolchain.toml' \
        ':(top)Dockerfile.companion' ':(top)src/**'
      ;;
  esac
  owned_status=0
  git diff --quiet "$before" "$after" -- "$@" || owned_status=$?
  if [ "$owned_status" -eq 1 ]; then
    printf '%s\n' "$after" >"$temporary/$kind"
  elif [ "$owned_status" -ne 0 ]; then
    fail owned_diff_failed
  fi
done

mv -- "$temporary" "$plan" || fail plan_publish_failed
trap - EXIT HUP INT TERM
echo "publisher_plan=ready"
