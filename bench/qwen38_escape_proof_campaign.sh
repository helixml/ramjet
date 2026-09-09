#!/usr/bin/env bash
# Escape-proof window: make engine B the single steered serving replica.
# On success the window EXITS LEAVING B steered + LB single-homed to B
# (live proof task runs in the flagged gap); any failure rolls back fully.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
baseline_image='vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'
lb_image='ghcr.io/helixml/ramjet:rust-0c7c7bc@sha256:f9215991a15a2d5ea223c84bfc4a2f7af423b0a3d4868423543c5d8a5315615f'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,http://qwen38flashnext-tp8:8000'
single_upstream='http://qwen38flashnext-b:8000'
all_speculation_profiles='standard,standard,standard'
single_speculation_profile='standard'
all_speculation_mode='off'
single_speculation_mode='off'
all_kv_live='tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557'
all_kv_replay='tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558'
single_kv_live='tcp://qwen38flashnext-b:5557'
single_kv_replay='tcp://qwen38flashnext-b:5558'

fail() {
  echo "qwen escape proof: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] ||
  fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_escape_proof_campaign.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] ||
  fail "canonical Compose bytes drifted"
candidate_compose=$experiment_dir/docker-compose.steer.yaml
plugin_image_id=$(cat "$experiment_dir/image-id.txt")
for artifact in steering-vector.safetensors "$candidate_compose" \
  "$experiment_dir/image-id.txt" "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  "$experiment_dir/capture_node06.sh" \
  "$experiment_dir/qwen38_escape_restore_campaign.sh"; do
  [[ -f $artifact ]] || fail "missing staged artifact: $artifact"
done
sha256sum \
  "$experiment_dir/steering-vector.safetensors" "$candidate_compose" \
  "$experiment_dir/image-id.txt" "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  "$experiment_dir/capture_node06.sh" \
  "$experiment_dir/qwen38_escape_restore_campaign.sh" \
  >"$experiment_dir/campaign-authority.sha256"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

capacity_for() {
  local ups=$1 n i out=""
  IFS=, read -ra parts <<< "$ups"
  n=${#parts[@]}
  for ((i = 0; i < n; i++)); do out+="${out:+,}-"; done
  printf '%s' "$out"
}

compose() {
  local file=$1 upstreams=$2 speculation_profiles=$3 speculation_mode=$4
  local kv_live=$5 kv_replay=$6
  shift 6
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    RJ_ROUTE_KV_CAPACITY_TOKENS="$(capacity_for "$upstreams")" \
    RJ_ROUTE_SPECULATION_PROFILES="$speculation_profiles" \
    RJ_ROUTE_SPECULATION_MODE="$speculation_mode" \
    RJ_KV_EVENT_LIVE_ENDPOINTS="$kv_live" \
    RJ_KV_EVENT_REPLAY_ENDPOINTS="$kv_replay" \
    docker compose -f "$file" --project-directory "$deployment_dir" "$@"
}

wait_engine() {
  local expected_image=$1 deadline=$((SECONDS + 900)) inspect
  until inspect=$(docker inspect "$engine" 2>/dev/null) &&
    jq -e --arg image "$expected_image" '
      length == 1 and
      (.[0].Image == $image or .[0].Config.Image == $image) and
      .[0].State.Status == "running" and .[0].State.OOMKilled == false and
      .[0].RestartCount == 0
    ' <<<"$inspect" >/dev/null &&
    curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
      http://127.0.0.1:8041/health >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_lb() {
  local expected_healthy=$1 expected_total=$2
  local deadline=$((SECONDS + 90)) health
  until health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null) &&
    jq -e --argjson healthy "$expected_healthy" --argjson total "$expected_total" '
      .status == "ok" and .healthy_replicas == $healthy and
      .active_replicas == $healthy and .total_replicas == $total
    ' <<<"$health" >/dev/null; do
    ((SECONDS < deadline)) || return 1
    if [[ $(docker inspect --format '{{.RestartCount}}' ds4-loadbalancer 2>/dev/null) != 0 ]]; then
      echo "wait_lb: load balancer is crash-looping (RestartCount != 0)" >&2
      docker logs --tail 20 ds4-loadbalancer >&2 2>&1 || true
      return 1
    fi
    sleep 2
  done
}

recreate_lb() {
  local file=$1 upstreams=$2 speculation_profiles=$3 speculation_mode=$4
  local kv_live=$5 kv_replay=$6 expected_healthy=$7 expected_total=$8
  compose "$file" "$upstreams" "$speculation_profiles" \
    "$speculation_mode" "$kv_live" "$kv_replay" \
    up -d --no-deps --force-recreate ds4-loadbalancer \
    >"$experiment_dir/lb-$expected_healthy-recreate.txt" 2>&1
  wait_lb "$expected_healthy" "$expected_total"
}

record_state() {
  local output=$1
  {
    date -u +%FT%TZ
    docker inspect --format \
      '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' \
      "$peer" "$engine"
    nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu \
      --format=csv,noheader
    curl -fsS --max-time 5 http://127.0.0.1:8006/health
  } >"$output"
}

lb_mutated=0
engine_mutated=0
success=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  if ((engine_mutated)); then
    compose "$canonical_compose" "$single_upstream" \
      "$single_speculation_profile" "$single_speculation_mode" \
      "$single_kv_live" "$single_kv_replay" \
      up -d --no-deps --force-recreate "$engine" \
      >"$experiment_dir/rollback-engine.txt" 2>&1 || rollback_rc=1
    wait_engine "$baseline_image" || rollback_rc=1
  fi
  if ((lb_mutated)); then
    recreate_lb "$canonical_compose" "$all_upstreams" "$all_speculation_profiles" \
      "$all_speculation_mode" "$all_kv_live" "$all_kv_replay" 2 3 ||
      rollback_rc=1
  fi
  record_state "$experiment_dir/final.txt" || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "qwen escape proof: rollback verification failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
record_state "$experiment_dir/initial.txt"

recreate_lb "$candidate_compose" "$single_upstream" "$single_speculation_profile" \
  "$single_speculation_mode" "$single_kv_live" "$single_kv_replay" 1 1
lb_mutated=1

engine_mutated=1
compose "$candidate_compose" "$single_upstream" "$single_speculation_profile" \
  "$single_speculation_mode" "$single_kv_live" "$single_kv_replay" \
  up -d --no-deps --force-recreate "$engine" \
  >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine "$plugin_image_id" || fail "steered engine did not become ready"

docker inspect "$engine" | jq -e --arg image "$plugin_image_id" '
  length == 1 and .[0].Image == $image and
  (.[0].Config.Env | any(. == "VLLM_PLUGINS=qwen38_steering")) and
  (.[0].Config.Env | any(startswith("QWEN38_STEERING_VECTOR="))) and
  (.[0].Config.Cmd | any(. == "--enforce-eager"))
' >/dev/null || fail "steering plugin is not admitted"

benign_smoke() {
  local label=$1 prompt=$2
  curl -fsS --max-time 120 -H "Authorization: Bearer $VLLM_API_KEY" \
    -H "Content-Type: application/json" \
    -d "$(jq -nc --arg m "$model" --arg p "$prompt" \
      '{model:$m,messages:[{role:"user",content:$p}],max_tokens:48,temperature:0}')" \
    http://127.0.0.1:8041/v1/chat/completions \
    | jq -r '.choices[0].message.content | gsub("[[:space:]]+";" ") | .[0:80]' \
    >"$experiment_dir/benign-$label.txt"
  [[ -s $experiment_dir/benign-$label.txt ]] || fail "benign smoke $label empty"
}
benign_smoke code-review 'Write a one-line Python function that returns the sum of a list of ints.'
benign_smoke report 'List three sections a security report usually has.'

printf '%s steered-live engine=%s image=%s\n' \
  "$(date -u +%FT%TZ)" "$engine" "$plugin_image_id" >"$experiment_dir/steered-live.flag"
success=1
record_state "$experiment_dir/ready.txt"
printf '%s\n' "proof window ready: LB single-homed to steered B; run the live task, then the restore campaign"
