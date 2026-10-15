#!/usr/bin/env bash
# Measure pinned NVIDIA Qwen A, then restore the live Swift A engine.
set -Eeuo pipefail
umask 077

deployment=/home/luke/inference/qwen38_flash_next
evidence=$deployment/.experiments/20260926T2100Z-swift15-reasoning
swift=$deployment/docker-compose.yaml
baseline=$deployment/.experiments/20260926T1930Z-swift15/baseline-compose-backup.yaml
swift_sha=9f81085ccaf2b385d32a58ae12aaf6fdfe2e7991b831fe6c6dca193dfbc7054e
baseline_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
image=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
mutated=0

fail() { echo "Swift reasoning A/B: $*" >&2; exit 2; }
intake() {
  curl -fsS --max-time 5 http://127.0.0.1:9100/metrics |
    awk '/^node_ipmi_temperature_celsius\{sensor="FP_TEMP"\}/ {value=$NF}
         END {if (value == "") exit 1; print value}'
}
check_sensors() {
  local air available limit
  air=$(intake) || fail "intake telemetry unavailable"
  awk -v air="$air" 'BEGIN {exit !(air < 50)}' || fail "intake reached 50C"
  available=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
  [[ $available =~ ^[0-9]+$ ]] || fail "memory telemetry invalid"
  ((available >= 8 * 1024 * 1024)) || fail "host memory below 8 GiB"
  for gpu in 0 1 2 3; do
    limit=$(nvidia-smi --id="$gpu" --query-gpu=power.limit --format=csv,noheader,nounits)
    [[ ${limit%%.*} == 450 ]] || fail "GPU $gpu power cap changed"
  done
}
lb_healthy() {
  curl -fsS --max-time 5 http://127.0.0.1:8006/health |
    jq -e '.status == "ok" and .healthy_replicas == 4 and .total_replicas == 4' >/dev/null
}
engine_healthy() {
  local repository=$1 revision=$2 model_source=$3
  docker inspect qwen38flashnext-a | jq -e --arg image "$image" \
    --arg repository "$repository" --arg revision "$revision" \
    --arg model_source "$model_source" '
    length == 1 and .[0].Image == $image and .[0].State.Running == true and
    .[0].State.OOMKilled == false and .[0].RestartCount == 0 and
    .[0].Config.Labels["ai.ramjet.model.repository"] == $repository and
    .[0].Config.Labels["ai.ramjet.model.revision"] == $revision and
    (.[0].Config.Cmd | index("--served-model-name=qwen3.8-flash-next") != null) and
    ([.[0].Mounts[] | select(.Source == $model_source and
      .Destination == "/workspace/model" and .RW == false)] | length == 1)
  ' >/dev/null &&
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8040/health >/dev/null
}
swift_healthy() {
  engine_healthy ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4 \
    3ff0520224f264a2d0ac4ab56ece8f2f13aadb38 \
    /prod/models/ukisai/Swift-1.5-Qwen3.8-Flash-Next-NVFP4-3ff0520224f264a2d0ac4ab56ece8f2f13aadb38
}
baseline_healthy() {
  engine_healthy nvidia/Qwen3.8-Flash-Next-NVFP4 \
    fc694b54fb0174e0913e6adf86691ef85a4ead47 \
    /prod/models/nvidia/Qwen3.8-Flash-Next-NVFP4-fc694b54fb0174e0913e6adf86691ef85a4ead47
}
wait_for() {
  local predicate=$1 thermal_check=${2:-yes} deadline=$((SECONDS + 1200))
  until "$predicate" && lb_healthy; do
    if [[ $thermal_check == yes ]]; then check_sensors; fi
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}
restore() {
  local original=$? restored=0
  trap - EXIT INT TERM
  set +e
  if ((mutated)); then
    docker compose -f "$swift" --project-directory "$deployment" \
      up -d --no-deps --force-recreate qwen38flashnext-a \
      >"$evidence/swift-restore.log" 2>&1 || restored=1
    wait_for swift_healthy no || restored=1
    docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
      --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
      >"$evidence/post-restore-identities.txt" 2>&1
  fi
  if ((restored)); then
    echo "Swift reasoning A/B: Swift restore failed; inspect $evidence" >&2
    exit 3
  fi
  if ((mutated)); then
    echo "Swift reasoning A/B: Swift restored with 4/4 healthy upstreams" >&2
  fi
  exit "$original"
}
trap restore EXIT
trap 'exit 130' INT TERM

[[ $(hostname) == node06 && $(id -u) == 0 ]] || fail "requires root on node06"
[[ $(stat -c '%u:%a' "$evidence") == 0:700 ]] || fail "evidence owner or mode changed"
[[ $(sha256sum "$swift" | cut -d' ' -f1) == "$swift_sha" ]] || fail "Swift Compose drifted"
[[ $(sha256sum "$baseline" | cut -d' ' -f1) == "$baseline_sha" ]] || fail "baseline Compose drifted"
[[ ! -e $evidence/baseline-reasoning.jsonl ]] || fail "baseline result already exists"
for pilot in swift-aime-pilot-r2 swift-aime-pilot-r3; do
  jq -se 'any(.[]; .type == "summary" and .completed == 1 and .correct == 1) and
    any(.[]; .status == "passed")' "$evidence/$pilot.jsonl" >/dev/null ||
    fail "Swift pilot $pilot did not complete correctly"
done
set -a
source "$deployment/.env"
set +a
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer invalid"
export BENCH_TOKEN=$VLLM_API_KEY
exec 9>/run/lock/ramjet-node06-deployment.lock
flock -n 9 || fail "deployment lock is held"
swift_healthy || fail "Swift production engine is not healthy"
lb_healthy || fail "shared LB is not 4/4 healthy"
for service in glm53sm120-b glm53sm120-c; do
  [[ $(docker inspect "$service" --format '{{.State.Running}}') == true ]] ||
    fail "$service is not running"
done
check_sensors
awk -v air="$(intake)" 'BEGIN {exit !(air <= 46)}' || fail "intake above launch ceiling"
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/pre-baseline-identities.txt"
docker compose -f "$baseline" --project-directory "$deployment" config --format json |
  jq -e '.services["qwen38flashnext-a"] as $a |
    $a.labels["ai.ramjet.model.repository"] == "nvidia/Qwen3.8-Flash-Next-NVFP4" and
    ($a.command | index("--served-model-name=qwen3.8-flash-next") != null) and
    ([ $a.volumes[] | select(.target == "/workspace/model" and .read_only == true and
      .source == "/prod/models/nvidia/Qwen3.8-Flash-Next-NVFP4-fc694b54fb0174e0913e6adf86691ef85a4ead47") ] |
      length == 1)' >/dev/null || fail "baseline render failed"

mutated=1
docker compose -f "$baseline" --project-directory "$deployment" \
  up -d --no-deps --force-recreate qwen38flashnext-a \
  >"$evidence/baseline-start.log" 2>&1
wait_for baseline_healthy || fail "baseline Qwen A did not become healthy"
docker inspect qwen38flashnext-a glm53sm120-b glm53sm120-c ds4-loadbalancer \
  --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' \
  >"$evidence/during-baseline-identities.txt"
cd "$evidence"
METRICS_URL=http://127.0.0.1:8040/metrics \
  python3 node06_gpu_guard.py --label qwen-baseline-aime-warmup \
    --output baseline-warmup-thermal.jsonl -- \
    python3 swift15_reasoning_perf.py http://127.0.0.1:8040 qwen3.8-flash-next \
      swift15_reasoning_aime2026_two.json --label baseline-warmup \
      --limit 1 --max-tokens 32768 --timeout 900 > baseline-warmup.jsonl
sleep 2
METRICS_URL=http://127.0.0.1:8040/metrics \
  python3 node06_gpu_guard.py --label qwen-baseline-aime-measurement \
    --output baseline-reasoning-thermal.jsonl -- \
    python3 swift15_reasoning_perf.py http://127.0.0.1:8040 qwen3.8-flash-next \
      swift15_reasoning_aime2026_matched.json --label baseline-measurement \
      --limit 2 --max-tokens 32768 --timeout 900 > baseline-reasoning.jsonl
jq -se 'any(.[]; .type == "summary" and .requests == 2) and
  any(.[]; .status == "passed")' baseline-reasoning.jsonl >/dev/null ||
  fail "baseline measurement/thermal journal is incomplete"
