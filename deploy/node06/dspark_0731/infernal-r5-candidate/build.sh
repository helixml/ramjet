#!/usr/bin/env bash
set -euo pipefail

candidate_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${candidate_root}/../../../.." && pwd)"
manifest="${candidate_root}/manifest.json"

r4_source=${R4_SOURCE:?set R4_SOURCE to the exact reconstructed r4 vLLM checkout}
mkdir -p "${repo_root}/target"
candidate_source=$(mktemp -d "${repo_root}/target/infernal-r5-source-XXXXXX")
rmdir "${candidate_source}"
trap 'rm -rf -- "${candidate_source}"' EXIT

python3 "${repo_root}/bench/infernal_candidate_overlay.py" \
  "${r4_source}" "${candidate_source}" --candidate-root "${candidate_root}"

# Source qualification is the safe default. Building requires an explicit opt
# in; this script deliberately has no push path.
if [[ ${BUILD_IMAGE:-0} != 1 ]]; then
  printf 'source preflight passed; set BUILD_IMAGE=1 to build locally\n'
  exit 0
fi

base_image=$(jq -er .base_image "${manifest}")
base_tree=$(jq -er .base_vllm_tree "${manifest}")
candidate_tree=$(jq -er .candidate_vllm_tree "${manifest}")
parser_id=$(jq -er .candidate_parser_source_id "${manifest}")
patch_sha256=$(jq -er .overlay_patch_sha256 "${manifest}")
cache_fingerprint=$(jq -er .cache_fingerprint "${manifest}")
image=${IMAGE:-ghcr.io/helixml/infernal-invocation:r5-v4-${candidate_tree:0:10}}

docker build \
  --network=none \
  --build-arg "BASE_IMAGE=${base_image}" \
  --build-arg "BASE_VLLM_TREE=${base_tree}" \
  --build-arg "CANDIDATE_VLLM_TREE=${candidate_tree}" \
  --build-arg "CANDIDATE_PARSER_SOURCE_ID=${parser_id}" \
  --build-arg "OVERLAY_PATCH_SHA256=${patch_sha256}" \
  --build-arg "CACHE_FINGERPRINT=${cache_fingerprint}" \
  --file "${candidate_root}/Dockerfile" \
  --tag "${image}" \
  "${candidate_root}"

test "$(docker image inspect "${image}" --format '{{index .Config.Labels "local-inference.vllm.integration.tree"}}')" = "${candidate_tree}"
test "$(docker image inspect "${image}" --format '{{index .Config.Labels "local-inference.overlay.patch-sha256"}}')" = "${patch_sha256}"
printf 'built=%s\n' "${image}"
