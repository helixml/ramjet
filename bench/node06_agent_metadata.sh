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

model_root=${BENCH_MODEL_ROOT:-/prod/models/DeepSeek-V4-Flash-0731}
for path in "$model_root/tokenizer.json" "$model_root/config.json"; do
  [[ -r $path ]] || { echo "missing model artifact: $path" >&2; exit 1; }
done

first_engine=$1
image_identity() {
  local container=$1 configured image_id
  configured=$(docker inspect --format '{{.Config.Image}}' "$container")
  image_id=$(docker inspect --format '{{.Image}}' "$container")
  if [[ $configured == *@sha256:* ]]; then
    printf '%s\n' "$configured"
  else
    printf '%s@%s\n' "$configured" "$image_id"
  fi
}
engine_image=$(for container in "$@"; do image_identity "$container"; done | sort -u | paste -sd, -)
router_version=${BENCH_ROUTER_VERSION:-$(image_identity ds4-loadbalancer)}
tokenizer_sha256=$(sha256sum "$model_root/tokenizer.json" | cut -d' ' -f1)
config_sha256=$(
  find "$model_root" -maxdepth 2 -type f \
    \( -name config.json -o -name generation_config.json -o -name tokenizer_config.json -o -name encoding_dsv4.py \) \
    -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1
)
# Probe a launcher's argv for an explicit --revision. pgrep exits non-zero
# when nothing matches and this script runs under `set -o pipefail`, so the
# probe must swallow that: an engine that is not vLLM is an ordinary case
# here, not a failure. Without the guard the script aborts before writing any
# metadata, which is how it failed against the SGLang Qwen3.8 stack.
#
# vLLM carries --revision. SGLang on that stack serves a bind-mounted model
# directory with no revision flag, so it legitimately finds nothing and falls
# through to the model-root identity below, which is the honest answer rather
# than an invented one.
probe_revision() {
  local pattern=$1
  { docker exec "$first_engine" pgrep -af "$pattern" || true; } | awk \
    '{for (field = 1; field <= NF; field++) if ($field == "--revision") {print $(field + 1); exit}}'
}
detected_revision=$(probe_revision 'vllm serve')
[[ -n $detected_revision ]] || detected_revision=$(probe_revision 'sglang.launch_server')

model_revision=${BENCH_MODEL_REVISION:-${detected_revision:-$(basename "$model_root")@$config_sha256}}

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
