#!/usr/bin/env bash
# Prove the pinned chat endpoint accepts the steering pair representation.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
[[ $(hostname) == node06 ]] || { echo "prefix probe: node06 only" >&2; exit 2; }
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || { echo "prefix probe: GPU guard is not active" >&2; exit 2; }
experiment_dir=$(realpath -e -- "${1:?usage: $0 EXPERIMENT-DIRECTORY}")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] || { echo "prefix probe: bad experiment path" >&2; exit 2; }
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || { echo "prefix probe: experiment directory is not private" >&2; exit 2; }
set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
export BENCH_TOKEN=${VLLM_API_KEY:?missing engine bearer authority}
python3 "$experiment_dir/qwen38_steering.py" probe \
  --base-url http://127.0.0.1:8041/v1 --model qwen3.8-flash-next \
  --pairs "$experiment_dir/pairs.json" --limit 1
