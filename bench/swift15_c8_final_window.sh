#!/usr/bin/env bash
# Repeat the isolated c8 cell after the Swift JIT cache has been primed.
set -Eeuo pipefail
umask 077

deployment=/home/luke/inference/qwen38_flash_next
original=$deployment/.experiments/20260926T1930Z-swift15
evidence=$deployment/.experiments/20260926T2021Z-swift15-c8-final
canonical=$deployment/docker-compose.yaml
candidate=$original/swift15-candidate.yaml
baseline_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
candidate_sha=7f6b274a9a8b74e290a126e46b1bfe02c9391af65de9b1d466d315dae2f02776
image=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
baseline_stopped=0
candidate_started=0

fail() { echo "Swift 1.5 c8 final: $*" >&2; exit 2; }

intake() {
  curl -fsS --max-time 5 http://127.0.0.1:9100/metrics |
    awk '/^node_ipmi_temperature_celsius\{sensor="FP_TEMP"\}/ {value=$NF}
         END {if (value == "") exit 1; print value}'
}

check_sensors() {
  local air available
  air=$(intake) || fail "intake telemetry is unavailable"
  [[ $air =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "intake telemetry is invalid"
  awk -v air="$air" 'BEGIN {exit !(air < 50)}' || fail "intake reached 50C"
  available=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
  [[ $available =~ ^[0-9]+$ ]] || fail "host memory telemetry is invalid"
  ((available >= 8 * 1024 * 1024)) || fail "host memory fell below 8 GiB"
}

baseline_health() {
  docker inspect qwen38flashnext-a | jq -e --arg image "$image" '
    length == 1 and .[0].Image == $image and .[0].State.Running == true and
    .[0].State.OOMKilled == false and .[0].RestartCount == 0 and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "nvidia/Qwen3.8-Flash-Next-NVFP4"
  ' >/dev/null &&
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8040/health >/dev/null
}

wait_baseline() {
  local deadline=$((SECONDS + 1000))
  until baseline_health && curl -fsS --max-time 5 http://127.0.0.1:8006/health |
    jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

rollback() {
  local original_status=$? restored=0
  trap - EXIT INT TERM
  set +e
  if ((candidate_started)); then
    docker logs --timestamps qwen38swift15-canary >"$evidence/candidate-final.log" 2>&1
    docker compose --profile swift15-canary -f "$candidate" \
      --project-directory "$deployment" rm -sf qwen38swift15-canary \
      >"$evidence/candidate-remove.log" 2>&1
    if docker inspect qwen38swift15-canary >/dev/null 2>&1; then
      docker rm -f qwen38swift15-canary >>"$evidence/candidate-remove.log" 2>&1
    fi
  fi
  if ((baseline_stopped)); then
    docker start qwen38flashnext-a >"$evidence/baseline-restore.log" 2>&1
    wait_baseline || restored=1
  fi
  if ((restored)); then
    echo "Swift 1.5 c8 final: baseline restore failed; inspect $evidence" >&2
    exit 3
  fi
  if ((baseline_stopped)); then
    echo "Swift 1.5 c8 final: baseline Qwen A restored" >&2
  fi
  exit "$original_status"
}
trap rollback EXIT
trap 'exit 130' INT TERM

[[ $(hostname) == node06 && $(id -u) == 0 ]] || fail "requires root on node06"
[[ $(stat -c '%u:%a' "$evidence") == 0:700 ]] || fail "evidence owner or mode changed"
[[ $(sha256sum "$canonical" | cut -d' ' -f1) == "$baseline_sha" ]] || fail "baseline Compose drifted"
[[ $(sha256sum "$candidate" | cut -d' ' -f1) == "$candidate_sha" ]] || fail "candidate Compose drifted"
jq -e '.verified == true and .revision == "3ff0520224f264a2d0ac4ab56ece8f2f13aadb38"' \
  "$original/model-verification.json" >/dev/null || fail "model verification is missing"
[[ ! -e $evidence/code-c8.jsonl && ! -e $evidence/warmup-c8.jsonl ]] || fail "final evidence already exists"
set -a
# shellcheck disable=SC1091
source "$deployment/.env"
set +a
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer is invalid"
export BENCH_TOKEN=$VLLM_API_KEY

exec 9>/run/lock/ramjet-node06-deployment.lock
flock -n 9 || fail "another node06 operation owns the deployment lock"
baseline_health || fail "baseline Qwen A is not healthy"
curl -fsS --max-time 5 http://127.0.0.1:8006/health |
  jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null ||
  fail "shared LB is not 4/4 healthy"
for service in glm53sm120-b glm53sm120-c; do
  [[ $(docker inspect "$service" --format '{{.State.Running}}') == true ]] ||
    fail "$service is not running"
done
[[ ! $(docker ps -a --filter name='^/qwen38swift15-canary$' --format '{{.ID}}') ]] ||
  fail "canary container already exists"
check_sensors
awk -v air="$(intake)" 'BEGIN {exit !(air <= 46)}' || fail "intake above 46C launch ceiling"
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/baseline-identities.txt"

baseline_stopped=1
docker stop --time 30 qwen38flashnext-a >"$evidence/baseline-stop.log"
for gpu in 0 1 2 3; do
  used=$(nvidia-smi --id="$gpu" --query-gpu=memory.used --format=csv,noheader,nounits)
  [[ $used =~ ^[[:space:]]*[0-9]+[[:space:]]*$ ]] || fail "GPU memory telemetry invalid"
  if ((gpu < 3)); then
    ((used < 1024)) || fail "GPU $gpu did not clear"
  else
    ((used < 8192)) || fail "GPU 3 has unexpected memory beyond KEV"
  fi
done
candidate_started=1
docker compose --profile swift15-canary -f "$candidate" \
  --project-directory "$deployment" up -d --no-deps qwen38swift15-canary \
  >"$evidence/candidate-start.log" 2>&1
deadline=$((SECONDS + 1200))
until curl -fsS --max-time 3 -H "Authorization: Bearer $VLLM_API_KEY" \
  http://127.0.0.1:8043/health >/dev/null 2>&1; do
  check_sensors
  [[ $(docker inspect qwen38swift15-canary --format '{{.State.Running}}') == true ]] ||
    fail "candidate exited during startup"
  ((SECONDS < deadline)) || fail "candidate readiness deadline expired"
  sleep 5
done
# First-use kernels compile on this process even when the disk cache exists.
# Warm the exact workload under the same thermal guard, then time the repeat.
METRICS_URL=http://127.0.0.1:8043/metrics BENCH_WORKLOAD=code \
  BENCH_SPEC_MODE=disabled BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
  python3 "$original/node06_gpu_guard.py" --label swift15-c8-final-warmup \
    --output "$evidence/warmup-c8-thermal.jsonl" -- \
    python3 "$original/codebench.py" http://127.0.0.1:8043 \
      qwen3.8-flash-next 256 8 3 | tee "$evidence/warmup-c8.jsonl"
jq -se 'any(.[]; .type == "engine_cell" and .requests_failed == 0 and
  .requests_ok == 24 and .dspark.reconciled == true) and
  any(.[]; .status == "passed")' "$evidence/warmup-c8.jsonl" >/dev/null ||
  fail "guarded warmup failed"
sleep 2
start_utc=$(date -u +%Y-%m-%dT%H:%M:%S)
METRICS_URL=http://127.0.0.1:8043/metrics BENCH_WORKLOAD=code \
  BENCH_SPEC_MODE=disabled BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
  python3 "$original/node06_gpu_guard.py" --label swift15-c8-final-measurement \
    --output "$evidence/code-c8-thermal.jsonl" -- \
    python3 "$original/codebench.py" http://127.0.0.1:8043 \
      qwen3.8-flash-next 256 8 3 | tee "$evidence/code-c8.jsonl"
end_utc=$(date -u +%Y-%m-%dT%H:%M:%S)
docker logs --timestamps qwen38swift15-canary >"$evidence/candidate-test.log" 2>&1
python3 - "$evidence/candidate-test.log" "$start_utc" "$end_utc" <<'PY'
import pathlib
import sys

log, start, end = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
jit = [line for line in log.read_text(errors="replace").splitlines()
       if "JIT compilation during inference" in line
       and start <= line[:19] <= end]
if jit:
    raise SystemExit("JIT occurred during the warm c8 retake")
PY
jq -se 'any(.[]; .type == "engine_cell" and .requests_failed == 0 and
  .requests_ok == 24 and .dspark.reconciled == true and .aggregate_tok_s_median >= 375) and
  any(.[]; .status == "passed")' "$evidence/code-c8.jsonl" >/dev/null ||
  fail "warm c8 throughput or reconciliation gate failed"
jq -cn --arg start "$start_utc" --arg end "$end_utc" \
  '{status:"passed",candidate:"Swift-1.5-Qwen3.8-Flash-Next-NVFP4",start_utc:$start,end_utc:$end,jit_during_cell:false}' \
  >"$evidence/retake-pass.json"
echo "Swift 1.5 c8 final: clean guarded cell passed; restoring baseline" >&2
