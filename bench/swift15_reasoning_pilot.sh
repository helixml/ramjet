#!/usr/bin/env bash
# Check whether a pinned xhigh question exercises end-to-end reasoning latency.
set -Eeuo pipefail
umask 077

deployment=/home/luke/inference/qwen38_flash_next
evidence=$deployment/.experiments/20260926T2100Z-swift15-reasoning
[[ $(hostname) == node06 && $(id -u) == 0 ]] || exit 2
[[ $(stat -c '%u:%a' "$evidence") == 0:700 ]] || exit 2
exec 9>/run/lock/ramjet-node06-deployment.lock
flock -n 9
set -a
source "$deployment/.env"
set +a
export BENCH_TOKEN=$VLLM_API_KEY
docker inspect qwen38flashnext-a | jq -e '
  .[0].State.Running == true and .[0].RestartCount == 0 and
  .[0].Config.Labels["ai.ramjet.model.revision"] ==
  "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"
' >/dev/null
curl -fsS --max-time 5 http://127.0.0.1:8006/health |
  jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null
for gpu in 0 1 2 3; do
  [[ $(nvidia-smi --id="$gpu" --query-gpu=power.limit --format=csv,noheader,nounits | cut -d. -f1) == 450 ]] || exit 2
done
cd "$evidence"
test ! -e swift-aime-pilot-r3.jsonl
METRICS_URL=http://127.0.0.1:8040/metrics \
  python3 node06_gpu_guard.py --label swift15-aime-reasoning-pilot-r3 \
    --output swift-aime-pilot-r3-thermal.jsonl -- \
    python3 swift15_reasoning_perf.py http://127.0.0.1:8040 qwen3.8-flash-next \
      swift15_reasoning_aime2026_matched.json --label swift-aime-pilot-r3 --limit 1 \
      --max-tokens 32768 --timeout 900 > swift-aime-pilot-r3.jsonl
jq -se 'any(.[]; .type == "summary" and .completed == 1 and .requests == 1) and
  any(.[]; .status == "passed")' swift-aime-pilot-r3.jsonl >/dev/null
