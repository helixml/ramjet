#!/usr/bin/env bash
# Detached outer owner for the thermal-guarded rollout. The guard may kill its
# whole candidate tree quickly; this parent remains outside that tree and
# performs an idempotent canonical Qwen-B restore on every non-zero result.
set -Eeuo pipefail

readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly qwen_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889

[[ $# == 1 ]] || { echo "usage: $0 EXISTING-EXPERIMENT-DIRECTORY" >&2; exit 2; }
experiment_dir=$(realpath -e -- "$1")
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] || { echo "invalid experiment directory" >&2; exit 2; }

set +e
EXPECTED_QWEN_COMPOSE_SHA256=$qwen_sha \
  python3 "$glm_dir/node06_gpu_guard.py" \
    --label glm53-sm120-v043-loader-smoke \
    --output "$experiment_dir/thermal.jsonl" \
    --runtime-start-signal \
    --runtime-start-timeout-seconds 2400 \
    --max-runtime-seconds 1500 \
    -- "$experiment_dir/node06-canary.sh" "$experiment_dir"
rc=$?
set -e

if ((rc != 0)); then
  echo "guarded rollout failed with status $rc; enforcing canonical Qwen-B restore" >&2
  EXPECTED_QWEN_COMPOSE_SHA256=$qwen_sha "$glm_dir/node06-restore-qwen-b.sh"
fi
exit "$rc"
