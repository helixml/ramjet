#!/usr/bin/env bash
# Recreate exactly one GLM replica from an exact Compose file while its peer
# and Qwen A keep serving. Run only beneath node06_gpu_guard.py with
# --runtime-start-signal: loading and graph capture happen before the
# inference budget starts. Failure stops only the candidate, so a thermal abort
# never initiates another model load; the peer replica keeps serving.
#
# The Compose file may be the canonical one or a full experiment copy whose
# only difference is an isolated default network, which keeps an unqualified
# candidate unreachable from the shared load balancer.
set -Eeuo pipefail

readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
readonly qwen_a=qwen38flashnext-a
readonly qwen_dir=/home/luke/inference/qwen38_flash_next

fail() { echo "GLM engine rollout: $*" >&2; exit 2; }

[[ $# == 5 ]] || fail "usage: $0 EXPERIMENT-DIR SERVICE COMPOSE-FILE COMPOSE-SHA256 IMAGE-ID"
[[ $(hostname) == node06 ]] || fail "this rollout may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
service=$2
compose=$(realpath -e -- "$3")
compose_sha=$4
image=$5
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] || fail "invalid experiment directory"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
case $service in
  glm53sm120-b) peer=glm53sm120-c port=8062 peer_port=8063 devices='["4","5"]' ;;
  glm53sm120-c) peer=glm53sm120-b port=8063 peer_port=8062 devices='["6","7"]' ;;
  *) fail "unknown GLM service" ;;
esac
[[ $(sha256sum "$compose" | awk '{print $1}') == "$compose_sha" ]] || fail "Compose bytes drifted"
[[ $(docker image inspect "$image" --format '{{.Id}}') == "$image" ]] || fail "candidate image is absent"

# shellcheck disable=SC1091
source "$qwen_dir/.env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

compose_cmd() { docker compose -f "$compose" --project-directory "$glm_dir" "$@"; }
qwen_health() {
  printf 'header = "Authorization: Bearer %s"\n' "$VLLM_API_KEY" |
    curl -fsS --config - --max-time 5 http://127.0.0.1:8040/health >/dev/null
}
health() { curl -fsS --max-time 5 "http://127.0.0.1:$1/health" >/dev/null; }
identity() { docker inspect "$1" --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}'; }
wait_ready() {
  local deadline=$((SECONDS + 2400)) inspect
  until inspect=$(docker inspect "$service" 2>/dev/null) &&
    jq -e --arg image "$image" --argjson devices "$devices" '
      length == 1 and .[0].Image == $image and
      .[0].State.Status == "running" and .[0].State.OOMKilled == false and
      .[0].RestartCount == 0 and
      .[0].HostConfig.DeviceRequests[0].DeviceIDs == $devices
    ' <<<"$inspect" >/dev/null && health "$port"; do
    [[ $(docker inspect "$service" --format '{{.State.Status}}' 2>/dev/null) != exited ]] ||
      return 1
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}
probe() {
  curl -fsS --max-time 120 -H 'Content-Type: application/json' \
    -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"Reply with one short word."}],"max_tokens":8,"temperature":0}' \
    "http://127.0.0.1:$port/v1/chat/completions" |
    jq -e '.model == "glm-5.3-flash" and .usage.completion_tokens >= 1' >/dev/null
}
start_inference_budget() {
  [[ ${RAMJET_GPU_GUARD_RUNTIME_START_FD:-} =~ ^[0-9]+$ ]] ||
    fail "runtime-start signal is unavailable"
  printf '1' >&"$RAMJET_GPU_GUARD_RUNTIME_START_FD" ||
    fail "runtime-start signal failed"
}
record() {
  docker inspect --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' "$qwen_a" "$peer" "$service" 2>&1 || true
  nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu,power.draw --format=csv,noheader
  grep -E 'MemAvailable|SwapFree' /proc/meminfo
}
save() { "$@" >"$experiment_dir/$service-$stage.txt" 2>&1; chmod 0600 "$experiment_dir/$service-$stage.txt"; }

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"
qwen_health || fail "Qwen A is not healthy"
health "$peer_port" || fail "peer $peer is not healthy; refusing to remove the last GLM replica"
qwen_before=$(identity "$qwen_a")
peer_before=$(identity "$peer")
stage=before save record

success=0
rollback() {
  local original_rc=$?
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  compose_cmd stop -t 5 "$service" >"$experiment_dir/$service-rollback.txt" 2>&1
  local rollback_rc=$?
  stage=final save record
  ((rollback_rc == 0)) || { echo "GLM engine rollout: candidate stop failed" >&2; exit 3; }
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

started=$SECONDS
compose_cmd up -d --no-deps --force-recreate "$service" >"$experiment_dir/$service-recreate.txt" 2>&1
wait_ready || fail "$service did not become ready"
printf '%s\n' "$((SECONDS - started))" >"$experiment_dir/$service-readiness-seconds.txt"
start_inference_budget
probe || fail "$service failed its direct smoke"
qwen_health || fail "Qwen A stopped serving"
health "$peer_port" || fail "peer $peer stopped serving"
[[ $(identity "$qwen_a") == "$qwen_before" ]] || fail "Qwen A identity changed"
[[ $(identity "$peer") == "$peer_before" ]] || fail "peer $peer identity changed"
stage=ready save record
docker inspect "$service" --format '{{json .Args}}' >"$experiment_dir/$service-args.json"
chmod 0600 "$experiment_dir/$service-args.json"
success=1
echo "GLM engine rollout: $service ready; Qwen A and $peer unchanged"
