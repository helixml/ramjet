#!/usr/bin/env bash
# Isolated node06 Swift 1.5 TP4 test; always restore the exact Qwen A container.
set -Eeuo pipefail
umask 077

deployment=/home/luke/inference/qwen38_flash_next
evidence=$deployment/.experiments/20260926T1930Z-swift15
canonical=$deployment/docker-compose.yaml
candidate=$evidence/swift15-candidate.yaml
model=/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38
baseline_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
candidate_sha=7f6b274a9a8b74e290a126e46b1bfe02c9391af65de9b1d466d315dae2f02776
image=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
revision=3ff0520224f264a2d0ac4ab56ece8f2f13aadb38
lock=/run/lock/ramjet-node06-deployment.lock
baseline_stopped=0
candidate_started=0

fail() { echo "Swift 1.5 canary: $*" >&2; exit 2; }

intake() {
  curl -fsS --max-time 5 http://127.0.0.1:9100/metrics |
    awk '/^node_ipmi_temperature_celsius\{sensor="FP_TEMP"\}/ {value=$NF}
         END {if (value == "") exit 1; print value}'
}

check_air() {
  local air
  air=$(intake) || fail "intake telemetry is unavailable"
  [[ $air =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "intake telemetry is invalid"
  awk -v air="$air" 'BEGIN {exit !(air < 50)}' || fail "intake reached 50C"
}

check_host_memory() {
  local available_kib
  available_kib=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
  [[ $available_kib =~ ^[0-9]+$ ]] || fail "host memory telemetry is invalid"
  ((available_kib >= 8 * 1024 * 1024)) || fail "host available memory fell below 8 GiB"
}

baseline_health() {
  docker inspect qwen38flashnext-a | jq -e --arg image "$image" '
    length == 1 and .[0].Image == $image and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "nvidia/Qwen3.8-Flash-Next-NVFP4" and
    .[0].Config.Labels["ai.ramjet.model.revision"] == "fc694b54fb0174e0913e6adf86691ef85a4ead47" and
    .[0].State.Running == true and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
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
  local original=$? restored=0
  trap - EXIT INT TERM
  set +e
  if ((candidate_started)); then
    docker logs --tail 250 qwen38swift15-canary >"$evidence/candidate-final.log" 2>&1
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
  docker inspect glm53sm120-b glm53sm120-c \
    --format '{{.Name}} {{.Id}} {{.State.Running}} {{.RestartCount}}' \
    >"$evidence/glm-final.txt" 2>&1
  if ((restored)); then
    echo "Swift 1.5 canary: baseline restore failed; inspect $evidence" >&2
    exit 3
  fi
  if ((baseline_stopped)); then
    echo "Swift 1.5 canary: baseline Qwen A restored" >&2
  fi
  exit "$original"
}
trap rollback EXIT
trap 'exit 130' INT TERM

[[ $(hostname) == node06 && $(id -u) == 0 ]] || fail "requires root on node06"
[[ $(stat -c '%u:%a' "$evidence") == 0:700 ]] || fail "evidence owner or mode changed"
[[ $(sha256sum "$canonical" | cut -d' ' -f1) == "$baseline_sha" ]] || fail "baseline Compose drifted"
[[ $(sha256sum "$candidate" | cut -d' ' -f1) == "$candidate_sha" ]] || fail "candidate Compose drifted"
[[ $(stat -c '%u:%a' "$candidate") == 0:600 ]] || fail "candidate Compose is not owner-only"
[[ -f $evidence/model-verification.json && -f $evidence/engine-args-preflight.txt ]] ||
  fail "model or parser preflight is missing"
jq -e --arg revision "$revision" '
  .verified == true and .revision == $revision and .files == 98 and
  .total_bytes == 186458010084 and
  .manifest_sha256 == "de956cabfc92ef5cd382e2557d03aa22d25fb57be3fe9048ed844d6863800287"
' "$evidence/model-verification.json" >/dev/null || fail "model authority differs"
grep -qx 'swift15_engine_args_preflight=passed' "$evidence/engine-args-preflight.txt" ||
  fail "parser preflight did not pass"
[[ ! -e $evidence/candidate-agent.jsonl && ! -e $evidence/candidate-code-c8.jsonl ]] ||
  fail "canary evidence already exists"
set -a
# shellcheck disable=SC1091
source "$deployment/.env"
set +a
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer is invalid"
export BENCH_TOKEN=$VLLM_API_KEY

exec 9>"$lock"
flock -n 9 || fail "another node06 operation owns the deployment lock"
baseline_health || fail "Qwen A baseline is not healthy"
curl -fsS --max-time 5 http://127.0.0.1:8006/health |
  jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null ||
  fail "shared LB is not 4/4 healthy"
for service in glm53sm120-b glm53sm120-c; do
  [[ $(docker inspect "$service" --format '{{.State.Running}}') == true ]] ||
    fail "$service is not running"
done
[[ ! $(docker ps -a --filter name='^/qwen38swift15-canary$' --format '{{.ID}}') ]] ||
  fail "canary container already exists"
check_air
awk -v air="$(intake)" 'BEGIN {exit !(air <= 46)}' || fail "intake above 46C launch ceiling"
check_host_memory
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/baseline-identities.txt"

baseline_stopped=1
docker stop --time 30 qwen38flashnext-a >"$evidence/baseline-stop.log"
for gpu in 0 1 2 3; do
  used=$(nvidia-smi --id="$gpu" --query-gpu=memory.used --format=csv,noheader,nounits)
  [[ $used =~ ^[[:space:]]*[0-9]+[[:space:]]*$ ]] || fail "GPU memory telemetry invalid"
  if ((gpu < 3)); then
    ((used < 1024)) || fail "GPU $gpu did not clear after Qwen stop"
  else
    ((used < 8192)) || fail "GPU 3 has unexpected memory beyond the KEV service"
  fi
done
check_air
candidate_started=1
docker compose --profile swift15-canary -f "$candidate" \
  --project-directory "$deployment" up -d --no-deps qwen38swift15-canary \
  >"$evidence/candidate-start.log" 2>&1
deadline=$((SECONDS + 1200))
until curl -fsS --max-time 3 -H "Authorization: Bearer $VLLM_API_KEY" \
  http://127.0.0.1:8043/health >/dev/null 2>&1; do
  check_air
  check_host_memory
  [[ $(docker inspect qwen38swift15-canary --format '{{.State.Running}}') == true ]] ||
    fail "candidate exited during startup"
  ((SECONDS < deadline)) || fail "candidate readiness deadline expired"
  sleep 5
done
docker inspect qwen38swift15-canary | jq -e --arg image "$image" --arg revision "$revision" '
  length == 1 and .[0].Image == $image and
  .[0].Config.Labels["ai.ramjet.model.revision"] == $revision and
  .[0].State.Running == true and .[0].State.OOMKilled == false and
  .[0].RestartCount == 0
' >/dev/null || fail "candidate runtime identity is wrong"
BENCH_GPU_COUNT=4 BENCH_MODEL_ROOT="$model" BENCH_MODEL_REVISION="$revision" \
  bash "$evidence/node06_agent_metadata.sh" "$evidence/candidate-metadata.json" \
  qwen38swift15-canary

cd "$evidence"
python3 node06_gpu_guard.py --label swift15-candidate-agent \
  --output candidate-agent-thermal.jsonl -- \
  python3 agentbench.py run http://127.0.0.1:8043 qwen3.8-flash-next \
    --corpus v1.jsonl --metadata-json candidate-metadata.json \
    --profile deterministic --engine-metrics http://127.0.0.1:8043/metrics \
    --speculation-mode disabled --require-reconciled-speculation --timeout 180 \
  | tee candidate-agent.jsonl
python3 node06_gpu_guard.py --label swift15-candidate-sessions \
  --output candidate-sessions-thermal.jsonl -- \
  python3 agentbench.py run http://127.0.0.1:8043 qwen3.8-flash-next \
    --corpus v2_sessions.jsonl --metadata-json candidate-metadata.json \
    --profile deterministic --engine-metrics http://127.0.0.1:8043/metrics \
    --speculation-mode disabled --require-reconciled-speculation --timeout 360 \
  | tee candidate-sessions.jsonl
python3 node06_gpu_guard.py --label swift15-candidate-image \
  --output candidate-image-thermal.jsonl -- \
  python3 multimodal_smoke.py http://127.0.0.1:8043 qwen3.8-flash-next \
    --engine-metrics http://127.0.0.1:8043/metrics \
    --spec-mode disabled --require-reconciled-speculation --timeout 180 \
  | tee candidate-image.jsonl
for concurrency in 1 8; do
  METRICS_URL=http://127.0.0.1:8043/metrics BENCH_WORKLOAD=code \
    BENCH_SPEC_MODE=disabled BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
    python3 node06_gpu_guard.py --label "swift15-candidate-code-c$concurrency" \
      --output "candidate-code-c$concurrency-thermal.jsonl" -- \
      python3 codebench.py http://127.0.0.1:8043 qwen3.8-flash-next 256 \
        "$concurrency" 3 | tee "candidate-code-c$concurrency.jsonl"
done
check_air
echo "Swift 1.5 canary: direct qualification complete; restoring baseline" >&2
