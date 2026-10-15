#!/usr/bin/env bash
# Promote the qualified Swift A engine without recreating the shared LB.
set -Eeuo pipefail
umask 077

deployment=/home/luke/inference/qwen38_flash_next
evidence=$deployment/.experiments/20260926T1930Z-swift15
retake=$deployment/.experiments/20260926T2021Z-swift15-c8-final
canonical=$deployment/docker-compose.yaml
production=$evidence/swift15-production.yaml
backup=$evidence/baseline-compose-backup.yaml
model=/prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38
baseline_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
production_sha=9f81085ccaf2b385d32a58ae12aaf6fdfe2e7991b831fe6c6dca193dfbc7054e
image=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
revision=3ff0520224f264a2d0ac4ab56ece8f2f13aadb38
mutated=0
committed=0

fail() { echo "Swift 1.5 promotion: $*" >&2; exit 2; }

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
  local available
  available=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
  [[ $available =~ ^[0-9]+$ ]] || fail "host memory telemetry is invalid"
  ((available >= 8 * 1024 * 1024)) || fail "host memory fell below 8 GiB"
}

lb_healthy() {
  curl -fsS --max-time 5 http://127.0.0.1:8006/health |
    jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null
}

baseline_healthy() {
  docker inspect qwen38flashnext-a | jq -e --arg image "$image" '
    length == 1 and .[0].Image == $image and .[0].State.Running == true and
    .[0].State.OOMKilled == false and .[0].RestartCount == 0 and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "nvidia/Qwen3.8-Flash-Next-NVFP4"
  ' >/dev/null &&
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8040/health >/dev/null
}

swift_healthy() {
  docker inspect qwen38flashnext-a | jq -e --arg image "$image" --arg revision "$revision" --arg model "$model" '
    length == 1 and .[0].Image == $image and .[0].State.Running == true and
    .[0].State.OOMKilled == false and .[0].RestartCount == 0 and
    .[0].Config.Labels["ai.ramjet.model.revision"] == $revision and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4" and
    (.[0].Config.Cmd | index("--served-model-name=qwen3.8-flash-next") != null) and
    ([.[0].Mounts[] | select(.Source == $model and .Destination == "/workspace/model" and .RW == false)] | length) == 1
  ' >/dev/null &&
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8040/health >/dev/null
}

wait_for() {
  local predicate=$1 thermal_check=${2:-yes} deadline=$((SECONDS + 1200))
  until "$predicate" && lb_healthy; do
    if [[ $thermal_check == yes ]]; then check_air; check_host_memory; fi
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

rollback() {
  local original=$? restored=0
  trap - EXIT INT TERM
  if ((committed)); then exit "$original"; fi
  set +e
  if ((mutated)); then
    docker logs --tail 250 qwen38flashnext-a >"$evidence/production-failure.log" 2>&1
    install -m 0644 "$backup" "$canonical.restore"
    mv -f "$canonical.restore" "$canonical"
    docker compose -f "$canonical" --project-directory "$deployment" \
      up -d --no-deps --force-recreate qwen38flashnext-a \
      >"$evidence/production-rollback.log" 2>&1
    wait_for baseline_healthy noair || restored=1
  fi
  if ((restored)); then
    echo "Swift 1.5 promotion: baseline rollback failed; inspect $evidence" >&2
    exit 3
  fi
  if ((mutated)); then
    echo "Swift 1.5 promotion: baseline Qwen A restored" >&2
  fi
  exit "$original"
}
trap rollback EXIT
trap 'exit 130' INT TERM

[[ $(hostname) == node06 && $(id -u) == 0 ]] || fail "requires root on node06"
[[ $production_sha =~ ^[0-9a-f]{64}$ ]] || fail "production Compose SHA is unpinned"
[[ $(stat -c '%u:%a' "$evidence") == 0:700 ]] || fail "evidence owner or mode changed"
[[ $(sha256sum "$canonical" | cut -d' ' -f1) == "$baseline_sha" ]] || fail "baseline Compose drifted"
[[ $(sha256sum "$production" | cut -d' ' -f1) == "$production_sha" ]] || fail "production Compose drifted"
[[ ! -e $backup ]] || fail "baseline backup already exists"
for file in candidate-agent.jsonl candidate-sessions.jsonl candidate-image.jsonl candidate-code-c1.jsonl; do
  [[ -f $evidence/$file ]] || fail "canary evidence is missing: $file"
  jq -se 'any(.[]; .status == "passed")' "$evidence/$file" >/dev/null ||
    fail "thermal guard did not pass: $file"
done
jq -se 'any(.[]; .type == "summary" and .protocol_valid == 5 and .requests == 5)' \
  "$evidence/candidate-agent.jsonl" >/dev/null || fail "agent/tool gate failed"
jq -se 'any(.[]; .type == "summary" and .protocol_valid == 7 and .requests == 7)' \
  "$evidence/candidate-sessions.jsonl" >/dev/null || fail "session/context gate failed"
jq -se 'any(.[]; .type == "multimodal_smoke" and .ok == true and .visual_answer_match == true)' \
  "$evidence/candidate-image.jsonl" >/dev/null || fail "image gate failed"
jq -se 'any(.[]; .type == "engine_cell" and .requests_failed == 0 and .requests_ok == 3 and .aggregate_tok_s_median >= 60)' \
  "$evidence/candidate-code-c1.jsonl" >/dev/null || fail "c1 throughput gate failed"
[[ -f $retake/retake-pass.json && -f $retake/code-c8.jsonl ]] ||
  fail "warm c8 retake evidence is missing"
jq -e '.status == "passed" and .jit_during_cell == false' \
  "$retake/retake-pass.json" >/dev/null || fail "warm c8 retake had JIT or failed"
jq -se 'any(.[]; .type == "engine_cell" and .requests_failed == 0 and
  .requests_ok == 24 and .dspark.reconciled == true and .aggregate_tok_s_median >= 375) and
  any(.[]; .status == "passed")' "$retake/code-c8.jsonl" >/dev/null ||
  fail "warm c8 throughput gate failed"

set -a
# shellcheck disable=SC1091
source "$deployment/.env"
set +a
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer is invalid"
export BENCH_TOKEN=$VLLM_API_KEY
exec 9>/run/lock/ramjet-node06-deployment.lock
flock -n 9 || fail "another node06 operation owns the deployment lock"
baseline_healthy || fail "baseline Qwen A is not healthy"
lb_healthy || fail "shared LB is not 4/4 healthy"
for service in glm53sm120-b glm53sm120-c; do
  [[ $(docker inspect "$service" --format '{{.State.Running}}') == true ]] ||
    fail "$service is not running"
done
check_air
check_host_memory
awk -v air="$(intake)" 'BEGIN {exit !(air <= 46)}' || fail "intake above 46C launch ceiling"
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/pre-promotion-identities.txt"
install -m 0600 "$canonical" "$backup"
install -m 0644 "$production" "$canonical.next"
mutated=1
mv -f "$canonical.next" "$canonical"
docker compose -f "$canonical" --project-directory "$deployment" \
  up -d --no-deps --force-recreate qwen38flashnext-a \
  >"$evidence/production-start.log" 2>&1
wait_for swift_healthy || fail "Swift production A did not become healthy"
cd "$evidence"
BENCH_GPU_COUNT=4 BENCH_MODEL_ROOT="$model" BENCH_MODEL_REVISION="$revision" \
  bash node06_agent_metadata.sh production-metadata.json qwen38flashnext-a
python3 node06_gpu_guard.py --label swift15-production-agent \
  --output production-agent-thermal.jsonl -- \
  python3 agentbench.py run http://127.0.0.1:8040 qwen3.8-flash-next \
    --corpus v1.jsonl --metadata-json production-metadata.json \
    --profile deterministic --timeout 180 \
  | tee production-agent.jsonl
jq -se 'any(.[]; .type == "summary" and .protocol_valid == 5 and .requests == 5) and any(.[]; .status == "passed")' \
  production-agent.jsonl >/dev/null || fail "production agent/tool gate failed"
lb_healthy || fail "shared LB is not 4/4 healthy after promotion"
swift_healthy || fail "Swift production identity changed"
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/post-promotion-identities.txt"
committed=1
echo "Swift 1.5 promotion: Qwen A serves the existing model name; LB remains 4/4 healthy" >&2
