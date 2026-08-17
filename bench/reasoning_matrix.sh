#!/usr/bin/env bash
# Bounded issue #14 reasoning-effort/output-budget matrix.
# Usage: reasoning_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]
set -euo pipefail

base=${1:?usage: reasoning_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
model=${2:?usage: reasoning_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
label=${3:?usage: reasoning_matrix.sh BASE MODEL LABEL [ENGINE_CONTAINER ...]}
shift 3
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
metadata=$(mktemp /tmp/ramjet-reasoning-metadata.XXXXXX.json)
trap 'rm -f "$metadata"' EXIT
matrix_started_ms=$(date +%s%3N)

"$script_dir/node06_agent_metadata.sh" "$metadata" "$@"
IFS=, read -r -a profiles <<<"${REASONING_PROFILES:-deterministic,agentic}"
IFS=, read -r -a efforts <<<"${REASONING_EFFORTS:-low,high,max}"
IFS=, read -r -a caps <<<"${REASONING_OUTPUT_CAPS:-96,192,256}"
concurrency=${REASONING_CONCURRENCY:-5}
runs=${REASONING_RUNS:-3}
prefix_kib=${REASONING_PREFIX_KIB:-0}
start_cell=${REASONING_START_CELL:-0}

for value in "$concurrency" "$runs"; do
  if ! [[ "$value" =~ ^[1-9][0-9]*$ ]]; then
    echo "concurrency and runs must be positive integers" >&2
    exit 2
  fi
done
for value in "$prefix_kib" "$start_cell"; do
  if ! [[ "$value" =~ ^[0-9]+$ ]]; then
    echo "prefix-kib and start-cell must be non-negative integers" >&2
    exit 2
  fi
done

cell=0
for profile in "${profiles[@]}"; do
  for effort in "${efforts[@]}"; do
    for cap in "${caps[@]}"; do
      if ((cell < start_cell)); then
        echo "reasoning-matrix skip cell=$cell profile=$profile effort=$effort cap=$cap" >&2
        cell=$((cell + 1))
        continue
      fi
      echo "reasoning-matrix start cell=$cell profile=$profile effort=$effort cap=$cap" >&2
      cell_started_ms=$(date +%s%3N)
      salt="${label}-cell${cell}-$(date +%s%N)"
      python3 "$script_dir/agentbench.py" run "$base" "$model" \
        --metadata-json "$metadata" --profile "$profile" \
        --label "${label}-${profile}-${effort}-m${cap}" \
        --prefix-kib "$prefix_kib" --salt "$salt" \
        --reasoning-effort "$effort" --max-output-tokens "$cap" \
        --report-protocol-failures \
        --concurrency "$concurrency" --repetitions "$runs"
      cell_finished_ms=$(date +%s%3N)
      jq -cn --arg label "$label" --arg profile "$profile" \
        --arg effort "$effort" --argjson cap "$cap" --argjson cell "$cell" \
        --argjson wall_ms "$((cell_finished_ms - cell_started_ms))" \
        '{type:"reasoning_cell_timing",label:$label,cell:$cell,profile:$profile,reasoning_effort:$effort,max_output_tokens:$cap,wall_ms:$wall_ms}'
      cell=$((cell + 1))
    done
  done
done

matrix_finished_ms=$(date +%s%3N)
jq -cn --arg label "$label" --argjson cells "$cell" \
  --argjson wall_ms "$((matrix_finished_ms - matrix_started_ms))" \
  '{type:"reasoning_matrix_timing",label:$label,cells:$cells,wall_ms:$wall_ms}'
