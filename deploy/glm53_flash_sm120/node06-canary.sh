#!/usr/bin/env bash
# Replace canonical Qwen B with one LB-isolated GLM TP2 canary. Run only as a
# child of node06_gpu_guard.py; any failure restores Qwen B before returning.
set -Eeuo pipefail

readonly qwen_dir=/home/luke/inference/qwen38_flash_next
readonly qwen_compose="$qwen_dir/docker-compose.yaml"
readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly glm_compose="$glm_dir/docker-compose.yaml"
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
readonly qwen_a=qwen38flashnext-a
readonly qwen_b=qwen38flashnext-b
readonly glm_b=glm53sm120-b
readonly qwen_image=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
readonly glm_ref=sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8
readonly model_dir=/prod/models/ormandj/GLM-5.3-Flash-W4A16-NVFP4-K32-Experts-FP8-WO-ee0989a944b0

fail() { echo "GLM SM120 canary: $*" >&2; exit 2; }

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
: "${EXPECTED_QWEN_COMPOSE_SHA256:?set exact operational Qwen Compose SHA-256}"

experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] || fail "experiment directory is outside the GLM deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/node06-canary.sh" ]] || fail "execute the staged campaign authority"
[[ $(sha256sum "$qwen_compose" | awk '{print $1}') == "$EXPECTED_QWEN_COMPOSE_SHA256" ]] || fail "Qwen Compose bytes drifted"
[[ $(sha256sum "$glm_compose" | awk '{print $1}') == 81ac5a53b83b7ace78882510fd06921467ecb899908e5ed6fd7c7d292f8eb314 ]] || fail "GLM Compose bytes drifted"
[[ $(sha256sum "$glm_dir/glm53-adaptive.json" | awk '{print $1}') == 23faa5c717d20bc1638148751ebdf1a474985ebf91fbc3ee4eacd7b6b1240722 ]] || fail "adaptive config bytes drifted"
[[ $(<"$glm_dir/model-verified.flag") == 'ee0989a944b0e213589191d7fca63af825a0741e verified' ]] || fail "model verification receipt missing"
[[ -f "$model_dir/config.json" ]] || fail "model config is missing"

glm_image=$(docker image inspect "$glm_ref" --format '{{.Id}}') || fail "exact candidate image is absent"
[[ $glm_image == sha256:* ]] || fail "candidate local image identity is invalid"
python3 "$glm_dir/validate-compose.py"

# shellcheck disable=SC1091
source "$qwen_dir/.env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

qwen_compose_cmd() { docker compose -f "$qwen_compose" --project-directory "$qwen_dir" "$@"; }
glm_compose_cmd() { docker compose -f "$glm_compose" --project-directory "$glm_dir" "$@"; }

direct_qwen_health() {
  local port=$1
  printf 'header = "Authorization: Bearer %s"\n' "$VLLM_API_KEY" |
    curl -fsS --config - --max-time 5 "http://127.0.0.1:${port}/health" >/dev/null
}

wait_qwen_b() {
  local deadline=$((SECONDS + 900)) inspect
  until inspect=$(docker inspect "$qwen_b" 2>/dev/null) &&
    jq -e --arg image "$qwen_image" 'length == 1 and .[0].Image == $image and .[0].State.Status == "running" and .[0].State.OOMKilled == false and .[0].RestartCount == 0' <<<"$inspect" >/dev/null &&
    direct_qwen_health 8041; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_glm_b() {
  local deadline=$((SECONDS + 2400)) inspect
  while ((SECONDS < deadline)); do
    if inspect=$(docker inspect "$glm_b" 2>/dev/null); then
      if jq -e --arg image "$glm_image" '
        length == 1 and .[0].Image == $image and
        .[0].State.Status == "running" and .[0].State.OOMKilled == false and
        .[0].RestartCount == 0 and
        .[0].HostConfig.DeviceRequests[0].DeviceIDs == ["4","5"] and
        (.[0].Config.Cmd | any(. == "--tp=2")) and
        (.[0].Config.Cmd | any(. == "--quantization=modelopt_mixed"))
      ' <<<"$inspect" >/dev/null && curl -fsS --max-time 5 http://127.0.0.1:8062/health >/dev/null; then
        return 0
      fi
      jq -e '.[0].State.Status == "exited" or .[0].State.Status == "dead"' <<<"$inspect" >/dev/null && return 1
    fi
    sleep 5
  done
  return 1
}

wait_lb_a_only() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --max-time 5 http://127.0.0.1:8007/metrics | awk '
    /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-a:8000"\} 1$/ {a=1}
    /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-b:8000"\} 0$/ {b=1}
    END {exit !(a && b)}'; do
    ((SECONDS < deadline)) || return 1
    sleep 3
  done
}

wait_lb_full() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --max-time 5 http://127.0.0.1:8006/health |
    jq -e '.healthy_replicas >= 2 and .replicas[0].healthy == true and .replicas[1].healthy == true' >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 3
  done
}

wait_qwen_b_idle() {
  local deadline=$((SECONDS + 300))
  until curl -fsS --max-time 5 http://127.0.0.1:8007/metrics | grep -Fx 'ramjet_upstream_inflight{upstream="http://qwen38flashnext-b:8000"} 0' >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

b_gpus_are_free() {
  local allowed uuid pid
  allowed=$(nvidia-smi --query-gpu=index,uuid --format=csv,noheader,nounits | awk -F ', ' '$1 >= 4 {print $2}')
  while IFS=', ' read -r uuid pid; do
    [[ -z $uuid ]] && continue
    if grep -Fxq "$uuid" <<<"$allowed"; then
      echo "B-side GPU still owned by compute PID $pid" >&2
      return 1
    fi
  done < <(nvidia-smi --query-compute-apps=gpu_uuid,pid --format=csv,noheader,nounits)
}

qwen_serve_probe() {
  printf 'header = "Authorization: Bearer %s"\n' "$VLLM_API_KEY" |
    curl -fsS --config - --max-time 60 -H 'Content-Type: application/json' \
    -d '{"model":"qwen3.8-flash-next","messages":[{"role":"user","content":"Reply with one short word."}],"max_tokens":4,"temperature":0}' \
    http://127.0.0.1:8006/v1/chat/completions | jq -e '.usage.completion_tokens >= 1' >/dev/null
}

glm_serve_probe() {
  curl -fsS --max-time 120 -H 'Content-Type: application/json' \
    -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"Reply with one short word."}],"max_tokens":8,"temperature":0}' \
    http://127.0.0.1:8062/v1/chat/completions | jq -e '.model == "glm-5.3-flash" and .usage.completion_tokens >= 1' >/dev/null
}

start_inference_budget() {
  [[ ${RAMJET_GPU_GUARD_RUNTIME_START_FD:-} =~ ^[0-9]+$ ]] ||
    fail "runtime-start signal is unavailable"
  printf '1' >&"$RAMJET_GPU_GUARD_RUNTIME_START_FD" ||
    fail "runtime-start signal failed"
}

record_state() {
  local output=$1
  {
    date -u +%FT%TZ
    for container in "$qwen_a" "$qwen_b" "$glm_b"; do
      docker inspect --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' "$container" 2>/dev/null || true
    done
    nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu,power.draw --format=csv,noheader
    curl -fsS --max-time 5 http://127.0.0.1:8006/health
  } >"$output"
  chmod 0600 "$output"
}

mutated=0
success=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  if ((mutated)); then
    glm_compose_cmd stop -t 120 "$glm_b" >"$experiment_dir/rollback-glm-stop.txt" 2>&1 || rollback_rc=1
    qwen_compose_cmd up -d --no-deps --force-recreate "$qwen_b" >"$experiment_dir/rollback-qwen-start.txt" 2>&1 || rollback_rc=1
    wait_qwen_b || rollback_rc=1
    wait_lb_full || rollback_rc=1
  fi
  record_state "$experiment_dir/final.txt" || rollback_rc=1
  ((rollback_rc == 0)) || { echo "GLM SM120 canary: rollback verification failed" >&2; exit 3; }
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

direct_qwen_health 8040 || fail "Qwen A is not healthy"
docker inspect "$qwen_a" "$qwen_b" | jq -e --arg image "$qwen_image" '
  length == 2 and ([.[].Image] | all(. == $image)) and
  ([.[].State.Status] | all(. == "running")) and
  ([.[].State.OOMKilled] | all(. == false)) and ([.[].RestartCount] | all(. == 0))
' >/dev/null || fail "Qwen A/B baseline identity is not admitted"
record_state "$experiment_dir/initial.txt"
qwen_a_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a")
wait_qwen_b_idle || fail "Qwen B did not drain before withdrawal"

mutated=1
qwen_compose_cmd stop -t 120 "$qwen_b" >"$experiment_dir/qwen-b-stop.txt" 2>&1
wait_lb_a_only || fail "shared LB did not isolate stopped Qwen B"
b_gpus_are_free || fail "candidate GPUs are not free after stopping Qwen B"
install -d -o root -g root -m 0700 /prod/engine-cache-sglang-glm53-sm120-v84-b

started=$SECONDS
glm_compose_cmd up -d --no-deps --force-recreate "$glm_b" >"$experiment_dir/glm-b-start.txt" 2>&1
wait_glm_b || fail "GLM B did not become ready"
printf '%s\n' "$((SECONDS - started))" >"$experiment_dir/glm-readiness-seconds.txt"
start_inference_budget
glm_serve_probe || fail "GLM B failed its deterministic direct request"
wait_lb_a_only || fail "GLM B became visible to the shared LB"
qwen_serve_probe || fail "Qwen A stopped serving while GLM B was isolated"
[[ $(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a") == "$qwen_a_before" ]] || fail "Qwen A identity changed"

docker logs "$glm_b" 2>&1 | grep -Ei 'loading model weights|model loading|cache|graph|JIT|CUDA|NCCL|Xid|OOM|traceback|fatal|ready to roll' | tail -n 500 >"$experiment_dir/glm-runtime-markers.txt" || true
chmod 0600 "$experiment_dir/glm-runtime-markers.txt"
record_state "$experiment_dir/ready.txt"
printf '%s\n' "$(date -u +%FT%TZ) glm-live engine=$glm_b port=8062 isolated=true qwen-a-serving=true" >"$experiment_dir/glm-live.flag"
success=1
echo "GLM TP2 is ready and LB-isolated on 127.0.0.1:8062; Qwen A is unchanged and serving"
