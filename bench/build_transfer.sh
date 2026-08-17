#!/usr/bin/env bash
# Build a Debian-compatible production image on the development machine and,
# optionally, stream it to node06. This keeps compilation off the GPU host.
set -euo pipefail

usage() {
  echo "usage: $0 TAG [--node06]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
TAG=$1
TRANSFER=${2:-}
[[ -z "$TRANSFER" || "$TRANSFER" == "--node06" ]] || usage
[[ "$TAG" =~ ^[A-Za-z0-9._-]+$ ]] || {
  echo "invalid image tag: $TAG" >&2
  exit 2
}

BUILDER=${RAMJET_BUILDER:-mini-dynamo-publisher}
REPOSITORY=${RAMJET_IMAGE_REPOSITORY:-ghcr.io/helixml/ds4-loadbalancer}
NODE=${RAMJET_NODE:-node06}
IMAGE="$REPOSITORY:$TAG"

docker buildx inspect "$BUILDER" >/dev/null 2>&1 || {
  echo "missing buildx builder '$BUILDER'" >&2
  echo "create it once: docker buildx create --name '$BUILDER' --driver docker-container --use" >&2
  exit 1
}

started=$(date +%s%N)
docker buildx build --builder "$BUILDER" --load -t "$IMAGE" .
built=$(date +%s%N)
size=$(docker image inspect "$IMAGE" --format '{{.Size}}')
printf 'image=%s size_bytes=%s build_wall_ms=%d\n' \
  "$IMAGE" "$size" "$(( (built - started) / 1000000 ))"

if [[ "$TRANSFER" == "--node06" ]]; then
  command -v zstd >/dev/null
  ssh "$NODE" 'command -v zstd >/dev/null'
  docker save "$IMAGE" | zstd -q -T0 -1 | \
    ssh "$NODE" 'zstd -q -d | docker load'
  transferred=$(date +%s%N)
  printf 'image=%s transfer_wall_ms=%d total_wall_ms=%d\n' \
    "$IMAGE" "$(( (transferred - built) / 1000000 ))" \
    "$(( (transferred - started) / 1000000 ))"
fi
