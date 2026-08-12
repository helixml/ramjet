#!/usr/bin/env bash
# Reproducible direct-engine DSpark decode matrix.
# Usage: engine_matrix.sh BASE MODEL LABEL
# Requires BENCH_TOKEN (or VLLM_API_KEY). METRICS_URL defaults to BASE/metrics.
set -euo pipefail

base=${1:?usage: engine_matrix.sh BASE MODEL LABEL}
model=${2:?usage: engine_matrix.sh BASE MODEL LABEL}
label=${3:?usage: engine_matrix.sh BASE MODEL LABEL}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

export METRICS_URL=${METRICS_URL:-${base%/}/metrics}
IFS=, read -r -a workloads <<<"${ENGINE_WORKLOADS:-code,prose}"
IFS=, read -r -a concurrencies <<<"${ENGINE_CONCURRENCIES:-1,8,16}"
matrix_started_ms=$(date +%s%3N)

for workload in "${workloads[@]}"; do
  [[ $workload == code || $workload == prose ]] || {
    echo "ENGINE_WORKLOADS entries must be code or prose" >&2
    exit 2
  }
  for concurrency in "${concurrencies[@]}"; do
    [[ $concurrency =~ ^[1-9][0-9]*$ ]] || {
      echo "ENGINE_CONCURRENCIES entries must be positive integers" >&2
      exit 2
    }
    if [[ -n ${ENGINE_RUNS:-} ]]; then
      runs=$ENGINE_RUNS
    elif [[ $concurrency == 1 ]]; then
      runs=5
    else
      runs=3
    fi
    [[ $runs =~ ^[1-9][0-9]*$ ]] || {
      echo "ENGINE_RUNS must be a positive integer" >&2
      exit 2
    }
    if [[ $concurrency == 1 ]]; then
      max_tokens=${ENGINE_C1_MAX_TOKENS:-512}
    else
      max_tokens=${ENGINE_CONCURRENT_MAX_TOKENS:-256}
    fi
    cell_started_ms=$(date +%s%3N)
    BENCH_WORKLOAD=$workload \
      SWEEP_LABEL=${label}-${workload}-c${concurrency} \
      python3 "$script_dir/codebench.py" \
        "$base" "$model" "$max_tokens" "$concurrency" "$runs"
    cell_finished_ms=$(date +%s%3N)
    jq -cn \
      --arg label "$label" --arg workload "$workload" \
      --argjson concurrency "$concurrency" \
      --argjson wall_ms "$((cell_finished_ms - cell_started_ms))" \
      '{type:"cell_timing",label:$label,workload:$workload,concurrency:$concurrency,wall_ms:$wall_ms}'
  done
done

matrix_finished_ms=$(date +%s%3N)
jq -cn --arg label "$label" --argjson wall_ms "$((matrix_finished_ms - matrix_started_ms))" \
  '{type:"matrix_timing",label:$label,wall_ms:$wall_ms}'
