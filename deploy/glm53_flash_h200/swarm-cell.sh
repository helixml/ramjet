#!/usr/bin/env bash
# swarm-cell.sh LABEL BASE [CONTAINER...] — one 64-developer swarm run.
set -euo pipefail
label=$1 base=$2; shift 2
R=${RESULTS:-${GLM_H200_ROOT:-$HOME}/results}
cd "$(dirname "$0")/../.."
python3 bench/agent_swarm_bench.py "$base" glm-5.3-flash --label "$label" \
  --developers "${DEVELOPERS:-64}" --duration "${DURATION:-900}" --warmup "${WARMUP:-180}" \
  --requests-jsonl "$R/swarm-requests.jsonl" 2>"$R/swarm-$label.progress" | tee -a "$R/swarm.jsonl"
for c in "$@"; do
  docker logs "$c" --since "$(( ${DURATION:-900} / 60 + 1 ))m" 2>&1 | grep "Decode batch" | awk -F"full token usage: " -v c="$c" \
    '{split($2,a,","); u=a[1]; s+=u; n++; if(u>m)m=u} END{printf "%s kv usage mean %.2f peak %.2f\n", c, s/n, m}'
done
