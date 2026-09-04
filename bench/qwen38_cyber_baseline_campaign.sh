#!/usr/bin/env bash
# Completion-private, synthetic cyber tool-readiness probe through the live LB.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
model=qwen3.8-flash-next

fail() {
  echo "qwen cyber baseline: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] ||
  fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_cyber_baseline_campaign.sh" ]] ||
  fail "execute the staged campaign authority"
for artifact in qwen38_cyber_eval.py qwen38_cyber_cases.json capture_node06.sh; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] ||
    fail "missing staged artifact: $artifact"
done

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export CYBER_EVAL_API_KEY=$VLLM_API_KEY

bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
python3 "$experiment_dir/qwen38_cyber_eval.py" run \
  --base-url http://127.0.0.1:8006/v1 --model "$model" \
  --cases "$experiment_dir/qwen38_cyber_cases.json" \
  --split train --split validation --split test --split safety \
  --concurrency 8 --max-tokens 96 --report-policy-failures \
  --output "$experiment_dir/baseline.json" \
  >"$experiment_dir/baseline-run.jsonl"
jq -c .summary "$experiment_dir/baseline.json"
