#!/usr/bin/env bash
# Generate privacy-safe provenance for agentbench results on node06.
# Usage: node06_agent_metadata.sh OUTPUT_JSON [ENGINE_CONTAINER ...]
set -euo pipefail

output=${1:?usage: node06_agent_metadata.sh OUTPUT_JSON [ENGINE_CONTAINER ...]}
shift
if (($# == 0)); then
  set -- dspark-0731 dspark-0731-b
fi
gpu_count=${BENCH_GPU_COUNT:-$((4 * $#))}

model_root=${BENCH_MODEL_ROOT:-/prod/models/sglang/DeepSeek-V4-Flash-0731}
for path in "$model_root/tokenizer.json" "$model_root/config.json"; do
  [[ -r $path ]] || { echo "missing model artifact: $path" >&2; exit 1; }
done

engine_image=$(docker inspect --format '{{.Config.Image}}' "$@" | sort -u | paste -sd, -)
router_version=${BENCH_ROUTER_VERSION:-$(docker inspect ds4-loadbalancer --format '{{.Config.Image}}')}
tokenizer_sha256=$(sha256sum "$model_root/tokenizer.json" | cut -d' ' -f1)
config_sha256=$(
  find "$model_root" -maxdepth 2 -type f \
    \( -name config.json -o -name generation_config.json -o -name tokenizer_config.json -o -name encoding_dsv4.py \) \
    -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1
)
model_revision=${BENCH_MODEL_REVISION:-$(basename "$model_root")@$config_sha256}

jq -n \
  --arg engine_image "$engine_image" \
  --arg model_revision "$model_revision" \
  --arg tokenizer_sha256 "$tokenizer_sha256" \
  --arg config_sha256 "$config_sha256" \
  --arg router_version "$router_version" \
  --argjson gpu_count "$gpu_count" \
  '{engine_image:$engine_image,model_revision:$model_revision,tokenizer_sha256:$tokenizer_sha256,config_sha256:$config_sha256,router_version:$router_version,gpu_count:$gpu_count}' \
  >"$output"
chmod 0600 "$output"
