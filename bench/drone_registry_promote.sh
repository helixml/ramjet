#!/bin/sh
# Copy one qualified SHA-tagged manifest to an immutable version tag.

set -eu

kind=${1-}
tag=${2-}
sha=${3-}
result=${4-release_publish}

case "$kind" in lb|companion) ;; *) echo "$result=error reason=invalid_publisher" >&2; exit 2 ;; esac
case "$result" in release_publish|release_recovery_publish) ;; *) exit 2 ;; esac

fail() {
  echo "$result=error reason=$1" >&2
  exit 2
}

[ "${#sha}" -eq 40 ] || fail invalid_revision
case "$sha" in *[!0-9a-fA-F]*) fail invalid_revision ;; esac
sha=$(printf '%s' "$sha" | tr 'A-F' 'a-f')
case "$tag" in v[0-9]*.[0-9]*.[0-9]*) ;; *) fail invalid_tag ;; esac
case "$tag" in *[!0-9A-Za-z._-]*) fail invalid_tag ;; esac
version=${tag#v}
short=$(printf '%.7s' "$sha")

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
    echo "$result=idempotent kind=$kind"
    exit 0
  fi
  fail destination_conflict
fi
case "$destination_result" in
  *MANIFEST_UNKNOWN*|*NAME_UNKNOWN*|*"manifest unknown"*|*"404 Not Found"*) ;;
  *) fail destination_lookup ;;
esac

crane copy "$source" "$destination" >/dev/null 2>&1 || fail copy_failed
destination_digest=$(crane digest "$destination" 2>/dev/null) || fail destination_digest
[ "$source_digest" = "$destination_digest" ] || fail digest_mismatch
echo "$result=complete kind=$kind"
