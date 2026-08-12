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

for workload in code prose; do
  for concurrency in 1 8 16; do
    if [[ $concurrency == 1 ]]; then
      max_tokens=512
      runs=5
    else
      max_tokens=256
      runs=3
    fi
    BENCH_WORKLOAD=$workload \
      SWEEP_LABEL=${label}-${workload}-c${concurrency} \
      python3 "$script_dir/codebench.py" \
        "$base" "$model" "$max_tokens" "$concurrency" "$runs"
  done
done
