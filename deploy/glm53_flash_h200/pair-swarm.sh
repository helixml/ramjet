#!/usr/bin/env bash
# pair-swarm.sh LABEL_A LABEL_B — simultaneous direct swarms, one per replica, identical structure.
set -euo pipefail
R=${RESULTS:-${GLM_H200_ROOT:-$HOME}/results}
cd "$(dirname "$0")/../.."
dev=${DEVELOPERS:-32}
# A multi-tokenizer SGLang start can hang in its own warm-up request and stay
# 503 forever, so readiness is bounded rather than awaited indefinitely.
deadline=$(( $(date +%s) + ${READY_TIMEOUT:-1200} ))
for port in 8070 8071; do
  until curl -sf -m5 "localhost:$port/health" >/dev/null; do
    (( $(date +%s) < deadline )) || { echo "replica on :$port not ready in ${READY_TIMEOUT:-1200}s" >&2; exit 1; }
    sleep 10
  done
done
D=deploy/glm53_flash_h200
if [[ "${SKIP_WARM:-0}" != 1 ]]; then
  (cd $D && ./serving-cell.sh "pairwarm-a-$(date +%s)" http://127.0.0.1:8070 48 96 16000 256 --random-range-ratio 0.05 >/dev/null) &
  (cd $D && ./serving-cell.sh "pairwarm-b-$(date +%s)" http://127.0.0.1:8071 48 96 16000 256 --random-range-ratio 0.05 >/dev/null) &
  wait
fi
python3 bench/agent_swarm_bench.py http://127.0.0.1:8070 glm-5.3-flash --label "$1" --developers "$dev" \
  --duration "${DURATION:-900}" --warmup "${WARMUP:-180}" --requests-jsonl "$R/swarm-requests.jsonl" \
  2>"$R/swarm-$1.progress" >>"$R/swarm.jsonl" &
python3 bench/agent_swarm_bench.py http://127.0.0.1:8071 glm-5.3-flash --label "$2" --developers "$dev" \
  --duration "${DURATION:-900}" --warmup "${WARMUP:-180}" --requests-jsonl "$R/swarm-requests.jsonl" \
  2>"$R/swarm-$2.progress" >>"$R/swarm.jsonl" &
wait
grep -E "\"label\": \"($1|$2)\"" "$R/swarm.jsonl"
