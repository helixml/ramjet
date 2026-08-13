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
version=${tag#v}
short=$(printf '%.7s' "$sha")
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || fail invalid_script_path

sh "$script_dir/drone_release_guard.sh" "$kind" >/dev/null || fail invalid_plan
command -v crane >/dev/null 2>&1 || fail missing_registry_client
[ -n "${GHCR_USERNAME-}" ] && [ -n "${GHCR_TOKEN-}" ] || fail missing_credentials
printf '%s' "$GHCR_TOKEN" | crane auth login ghcr.io -u "$GHCR_USERNAME" --password-stdin >/dev/null 2>&1 \
  || fail registry_auth

case "$kind" in
  lb)
    source="ghcr.io/helixml/mini-dynamo:rust-$short"
    destination="ghcr.io/helixml/mini-dynamo:$tag"
    ;;
  companion)
    source="ghcr.io/helixml/mini-dynamo:companion-rust-$short"
    destination="ghcr.io/helixml/mini-dynamo:companion-$tag"
    ;;
esac

source_digest=$(crane digest "$source" 2>/dev/null) || fail source_digest
config=$(crane config "$source" 2>/dev/null) || fail source_missing
compact=$(printf '%s' "$config" | tr -d '[:space:]')
printf '%s' "$compact" | grep -Fq \
  "\"org.opencontainers.image.source\":\"https://github.com/helixml/mini-dynamo\"" \
  || fail source_label_mismatch
printf '%s' "$compact" | grep -Fq \
  "\"org.opencontainers.image.version\":\"$version\"" \
  || fail version_label_mismatch
printf '%s' "$compact" | grep -Fq \
  "\"org.opencontainers.image.revision\":\"$sha\"" \
  || fail revision_label_mismatch

destination_result=
if destination_result=$(crane digest "$destination" 2>&1); then
  if [ "$destination_result" = "$source_digest" ]; then
    echo "release_publish=idempotent kind=$kind"
    exit 0
  fi
  fail destination_conflict
fi
case "$destination_result" in
  *MANIFEST_UNKNOWN*|*NAME_UNKNOWN*|*"manifest unknown"*|*"404 Not Found"*|*"not found"*) ;;
  *) fail destination_lookup ;;
esac

crane copy "$source" "$destination" >/dev/null 2>&1 || fail copy_failed
destination_digest=$(crane digest "$destination" 2>/dev/null) || fail destination_digest
[ "$source_digest" = "$destination_digest" ] || fail digest_mismatch
echo "release_publish=complete kind=$kind"
