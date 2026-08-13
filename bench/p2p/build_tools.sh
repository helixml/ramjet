#!/usr/bin/env bash
set -euo pipefail

readonly NVBANDWIDTH_SHA=82fc4e8c6afa0babb8687793678f615b3b8d793e
readonly NCCL_TESTS_SHA=717b68318278e93f371d8ffb46b076069d7c7851
readonly R34_REPO_DIGEST=voipmonitor/vllm@sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b
readonly SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

usage() {
  cat <<'EOF'
Usage: build_tools.sh OUTPUT_DIR

Builds pinned NVIDIA nvbandwidth and nccl-tests binaries on the development
machine. OUTPUT_DIR must not already contain files. The script never connects
to node06 and never runs a GPU workload.
EOF
}

[[ $# -eq 1 ]] || { usage >&2; exit 2; }
output_dir=$1
[[ $output_dir == /* ]] || { echo "OUTPUT_DIR must be absolute" >&2; exit 2; }
mkdir -p -- "$output_dir"
[[ -z $(find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit) ]] || {
  echo "OUTPUT_DIR must be empty" >&2
  exit 2
}

build_root=$(mktemp -d "${TMPDIR:-/tmp}/mini-dynamo-p2p-build.XXXXXX")
cleanup() {
  case $build_root in
    */mini-dynamo-p2p-build.*) rm -rf -- "$build_root" ;;
    *) echo "refusing to clean unexpected build path" >&2 ;;
  esac
}
trap cleanup EXIT

fetch_exact() {
  local repository=$1
  local commit=$2
  local destination=$3
  git init -q "$destination"
  git -C "$destination" remote add origin "$repository"
  git -C "$destination" fetch -q --depth=1 origin "$commit"
  git -C "$destination" checkout -q --detach FETCH_HEAD
  [[ $(git -C "$destination" rev-parse HEAD) == "$commit" ]] || {
    echo "source identity mismatch for $repository" >&2
    exit 1
  }
}

fetch_exact https://github.com/NVIDIA/nvbandwidth.git "$NVBANDWIDTH_SHA" \
  "$build_root/nvbandwidth"
fetch_exact https://github.com/NVIDIA/nccl-tests.git "$NCCL_TESTS_SHA" \
  "$build_root/nccl-tests"
cp -- "$SCRIPT_DIR/Dockerfile.tools" "$build_root/Dockerfile"

docker buildx build \
  --file "$build_root/Dockerfile" \
  --build-arg "R34_IMAGE=$R34_REPO_DIGEST" \
  --target export \
  --output "type=local,dest=$output_dir" \
  "$build_root"

chmod 0555 "$output_dir/nvbandwidth" "$output_dir/all_reduce_perf"
nvbandwidth_hash=$(sha256sum "$output_dir/nvbandwidth" | awk '{print $1}')
nccl_tests_hash=$(sha256sum "$output_dir/all_reduce_perf" | awk '{print $1}')

python3 - "$output_dir/manifest.json" \
  "$NVBANDWIDTH_SHA" "$NCCL_TESTS_SHA" "$R34_REPO_DIGEST" \
  "$nvbandwidth_hash" "$nccl_tests_hash" <<'PY'
import json
import os
import sys

path, nvbandwidth, nccl_tests, image, nv_hash, nccl_hash = sys.argv[1:]
document = {
    "schema_version": 1,
    "nvbandwidth_commit": nvbandwidth,
    "nccl_tests_commit": nccl_tests,
    "runtime_image": image,
    "cuda_architecture": "120",
    "binaries": {
        "nvbandwidth": {"sha256": nv_hash},
        "all_reduce_perf": {"sha256": nccl_hash},
    },
}
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o444)
with os.fdopen(fd, "w", encoding="utf-8") as handle:
    json.dump(document, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

echo "pinned P2P tools built in $output_dir"
sha256sum "$output_dir/manifest.json" "$output_dir/nvbandwidth" \
  "$output_dir/all_reduce_perf"
