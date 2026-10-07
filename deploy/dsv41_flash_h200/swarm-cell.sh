#!/usr/bin/env bash
# swarm-cell.sh LABEL [BASE] — one coding-agent swarm run through ramjet.
set -euo pipefail
label=$1 base=${2:-http://127.0.0.1:8006}
R=${RESULTS:-${DS_H200_ROOT:-$HOME}/results}
mkdir -p "$R"
cd "$(dirname "$0")/../.."
python3 bench/agent_swarm_bench.py "$base" deepseek-v4.1-flash --label "$label" \
  --developers "${DEVELOPERS:-64}" --duration "${DURATION:-300}" --warmup "${WARMUP:-60}" \
  --requests-jsonl "$R/swarm-requests.jsonl" 2>"$R/swarm-$label.progress" | tee -a "$R/swarm.jsonl"
