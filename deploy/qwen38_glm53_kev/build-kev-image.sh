#!/usr/bin/env bash
set -Eeuo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
source_dir=${KEV_SOURCE_DIR:-/home/karolis/go/src/github.com/jaredpalmer/kev}
source_revision=5e94a28818cfd3d0ec9b8bca046dc8db0d79a704
model_revision=54f4f8777356cd5bbbb6c6919c657f26e6f2f6d8
image=${KEV_IMAGE_TAG:-ghcr.io/helixml/ramjet-kev:0.8b-${source_revision:0:7}-r2}

[[ -d "$source_dir/.git" ]] || {
  echo "KEV_SOURCE_DIR must be a Kev Git checkout" >&2
  exit 2
}
[[ -z $(git -C "$source_dir" status --porcelain) ]] || {
  echo "Kev source checkout must be clean" >&2
  exit 2
}
[[ $(git -C "$source_dir" rev-parse HEAD) == "$source_revision" ]] || {
  echo "Kev source checkout is not pinned to $source_revision" >&2
  exit 2
}

docker buildx build --load \
  --build-arg "KEV_SOURCE_REVISION=$source_revision" \
  --build-arg "KEV_MODEL_REVISION=$model_revision" \
  -f "$root/Dockerfile.kev" \
  -t "$image" \
  "$source_dir"

docker image inspect "$image" --format 'image={{index .RepoTags 0}} id={{.Id}} size_bytes={{.Size}}'
