#!/usr/bin/env bash
# Add the second GLM TP2 replica on GPUs 6-7 without recreating Qwen, the
# established GLM replica, or Ramjet. Run this script through node06_gpu_guard.py.
set -Eeuo pipefail

readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly compose="$glm_dir/docker-compose.yaml"
readonly secret_env=/home/luke/inference/qwen38_flash_next/.env
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
readonly qwen=qwen38flashnext-a
readonly glm_b=glm53sm120-b
readonly glm_c=glm53sm120-c
readonly glm_c_cache=/prod/engine-cache-sglang-glm53-sm120-v84-c
readonly fixed_image=sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8
readonly compose_sha=765452df91208722d8deb166ce96bf834262093201983e252b1b99a8a8037483

fail() { echo "second GLM replica rollout: $*" >&2; exit 2; }

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this rollout may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] || fail "invalid experiment directory"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/node06-second-replica-rollout.sh" ]] || fail "execute the staged rollout authority"
[[ $(sha256sum "$compose" | awk '{print $1}') == "$compose_sha" ]] || fail "GLM Compose bytes drifted"
[[ -f $secret_env && ! -L $secret_env && $(stat -c %a "$secret_env") == 600 ]] || fail "protected engine environment is invalid"
[[ $(docker image inspect "$fixed_image" --format '{{.Id}}') == "$fixed_image" ]] || fail "fixed image is absent"
python3 "$glm_dir/validate-compose.py"

# shellcheck disable=SC1090
source "$secret_env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

compose_run() {
  docker compose -f "$compose" --project-directory "$glm_dir" "$@"
}
qwen_health() {
  printf 'header = "Authorization: Bearer %s"\n' "$VLLM_API_KEY" |
    curl -fsS --config - --max-time 5 http://127.0.0.1:8040/health >/dev/null
}
glm_health() { curl -fsS --max-time 5 "http://127.0.0.1:$1/health" >/dev/null; }
lb_health() { curl -fsS --max-time 5 http://127.0.0.1:8006/health >/dev/null; }
identity() {
  docker inspect "$1" --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}'
}
record() {
  docker inspect --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' \
    "$qwen" "$glm_b" "$glm_c"
  nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu,power.draw,power.limit \
    --format=csv,noheader
  free -h
}
start_inference_budget() {
  [[ ${RAMJET_GPU_GUARD_RUNTIME_START_FD:-} =~ ^[0-9]+$ ]] || fail "runtime-start signal is unavailable"
  printf '1' >&"$RAMJET_GPU_GUARD_RUNTIME_START_FD" || fail "runtime-start signal failed"
}
glm_c_probe() {
  curl -fsS --max-time 120 -H 'Content-Type: application/json' \
    -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"Reply with one short word."}],"max_tokens":8,"temperature":0}' \
    http://127.0.0.1:8063/v1/chat/completions |
    jq -e '.model == "glm-5.3-flash" and .usage.completion_tokens >= 1' >/dev/null
}

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"
qwen_health || fail "Qwen A is not healthy"
glm_health 8062 || fail "established GLM B is not healthy"
lb_health || fail "Ramjet is not healthy"
[[ $(docker inspect "$qwen" --format '{{.RestartCount}}') == 0 ]] || fail "Qwen A has restarted"
[[ $(docker inspect "$glm_b" --format '{{.RestartCount}}') == 0 ]] || fail "established GLM B has restarted"
[[ $(docker inspect "$glm_b" --format '{{.Image}}') == "$fixed_image" ]] || fail "established GLM B image drifted"
if docker inspect "$glm_c" >/dev/null 2>&1; then
  fail "candidate container already exists"
fi
if ss -ltnH 'sport = :8063' | grep -q .; then
  fail "candidate loopback port 8063 is occupied"
fi
mem_available_kib=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
((mem_available_kib >= 48 * 1024 * 1024)) || fail "less than 48 GiB host memory is available"
nvidia-smi --query-gpu=index,memory.used,power.limit --format=csv,noheader,nounits -i 6,7 |
  awk -F ', ' '$2 != 0 || $3 != 600 {bad=1} END {exit bad}' ||
  fail "GPUs 6-7 are not empty at the admitted 600W inference ceiling"

qwen_before=$(identity "$qwen")
glm_b_before=$(identity "$glm_b")
lb_before=$(identity ds4-loadbalancer)
install -d -m 0700 "$glm_c_cache"
record >"$experiment_dir/before.txt"
chmod 0600 "$experiment_dir/before.txt"

success=0
rollback() {
  local original_rc=$?
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  docker logs --tail 400 "$glm_c" >"$experiment_dir/candidate.log" 2>&1
  chmod 0600 "$experiment_dir/candidate.log" 2>/dev/null || true
  compose_run stop -t 5 "$glm_c" >"$experiment_dir/candidate-stop.txt" 2>&1
  compose_run rm -f "$glm_c" >>"$experiment_dir/candidate-stop.txt" 2>&1
  record >"$experiment_dir/final.txt" 2>&1 || true
  chmod 0600 "$experiment_dir/candidate-stop.txt" "$experiment_dir/final.txt" 2>/dev/null || true
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

started=$SECONDS
compose_run up -d --no-deps "$glm_c" >"$experiment_dir/create.txt" 2>&1
ready=0
while ((SECONDS - started < 2400)); do
  inspect=$(docker inspect "$glm_c" 2>/dev/null || true)
  if jq -e --arg image "$fixed_image" '
    length == 1 and .[0].Image == $image and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0 and
    .[0].HostConfig.DeviceRequests[0].DeviceIDs == ["6","7"] and
    (.[0].Config.Cmd | any(. == "--tp=2")) and
    (.[0].Config.Cmd | any(. == "--max-total-tokens=500000")) and
    (.[0].Config.Cmd | any(. == "--chunked-prefill-size=6144")) and
    (.[0].Config.Cmd | any(. == "--max-prefill-tokens=6144"))
  ' <<<"${inspect:-[]}" >/dev/null && glm_health 8063; then
    ready=1
    break
  fi
  if jq -e 'length == 1 and .[0].State.Status != "running"' <<<"${inspect:-[]}" >/dev/null; then
    fail "candidate exited before readiness"
  fi
  sleep 5
done
[[ $ready == 1 ]] || fail "candidate did not become ready"
printf '%s\n' "$((SECONDS - started))" >"$experiment_dir/readiness-seconds.txt"

# Model loading, JIT, and graph capture are excluded from this short inference
# budget. Only the direct acceptance request runs after the signal.
start_inference_budget
glm_c_probe || fail "candidate failed its direct inference smoke"
qwen_health || fail "Qwen A stopped serving"
glm_health 8062 || fail "established GLM B stopped serving"
lb_health || fail "Ramjet stopped serving"
[[ $(identity "$qwen") == "$qwen_before" ]] || fail "Qwen A identity changed"
[[ $(identity "$glm_b") == "$glm_b_before" ]] || fail "established GLM B identity changed"
[[ $(identity ds4-loadbalancer) == "$lb_before" ]] || fail "Ramjet identity changed"
record >"$experiment_dir/ready.txt"
chmod 0600 "$experiment_dir/ready.txt" "$experiment_dir/readiness-seconds.txt"
success=1
echo "second GLM TP2 replica is ready on GPUs 6-7; Qwen, GLM B, and Ramjet are unchanged"
