#!/usr/bin/env bash
# Production-shaped agent protocol matrix. Keep direct-engine A/B cells parallel;
# keep full-LB capacity/locality cells serial because they share routing state.
# Usage: agent_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]
set -euo pipefail

base=${1:?usage: agent_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
model=${2:?usage: agent_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
label=${3:?usage: agent_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
shift 3
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
metrics_args=()
if [[ -n ${AGENT_ENGINE_METRICS:-} ]]; then
  metrics_args+=(--engine-metrics "$AGENT_ENGINE_METRICS")
fi
if [[ ${AGENT_REQUIRE_RECONCILED_SPECULATION:-0} == 1 ]]; then
  if [[ ${#metrics_args[@]} == 0 ]]; then
    echo "AGENT_REQUIRE_RECONCILED_SPECULATION=1 requires AGENT_ENGINE_METRICS" >&2
    exit 2
  fi
  metrics_args+=(--require-reconciled-speculation)
fi
metadata=$(mktemp /tmp/mini-dynamo-agent-metadata.XXXXXX.json)
trap 'rm -f "$metadata"' EXIT
matrix_started_ms=$(date +%s%3N)

"$script_dir/node06_agent_metadata.sh" "$metadata" "$@"
IFS=, read -r -a profiles <<<"${AGENT_PROFILES:-deterministic,agentic}"
IFS=, read -r -a prefix_sizes <<<"${AGENT_PREFIX_KIBS:-0,256}"
IFS=, read -r -a concurrencies <<<"${AGENT_CONCURRENCIES:-1,8,16}"
runs=${AGENT_RUNS:-1}

for profile in "${profiles[@]}"; do
  for prefix_kib in "${prefix_sizes[@]}"; do
    for concurrency in "${concurrencies[@]}"; do
      repetitions=$(((concurrency + 4) / 5 * runs))
      salt="${label}-${profile}-p${prefix_kib}-c${concurrency}-$(date +%s%N)"
      python3 "$script_dir/agentbench.py" run "$base" "$model" \
        --metadata-json "$metadata" --profile "$profile" \
        --label "${label}-cold-p${prefix_kib}-c${concurrency}" \
        --prefix-kib "$prefix_kib" --salt "$salt" \
        --concurrency "$concurrency" --repetitions "$repetitions" \
        "${metrics_args[@]}"
      python3 "$script_dir/agentbench.py" run "$base" "$model" \
        --metadata-json "$metadata" --profile "$profile" \
        --label "${label}-warm-p${prefix_kib}-c${concurrency}" \
        --prefix-kib "$prefix_kib" --salt "$salt" --warmup \
        --concurrency "$concurrency" --repetitions "$repetitions" \
        "${metrics_args[@]}"
    done
  done
done

matrix_finished_ms=$(date +%s%3N)
jq -cn --arg label "$label" --argjson wall_ms "$((matrix_finished_ms - matrix_started_ms))" \
  '{type:"matrix_timing",label:$label,wall_ms:$wall_ms}'
