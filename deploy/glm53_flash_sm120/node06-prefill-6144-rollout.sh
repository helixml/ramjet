#!/usr/bin/env bash
# Recreate only the isolated GLM service with 6K prefill chunks. This is an
# experiment owner: Qwen A is observation-only and a failed candidate is
# stopped rather than starting another model load during a thermal abort.
set -Eeuo pipefail

readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly compose="$glm_dir/docker-compose.yaml"
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
readonly glm=glm53sm120-b
readonly qwen_a=qwen38flashnext-a
readonly qwen_dir=/home/luke/inference/qwen38_flash_next
readonly fixed_image=sha256:024a988fd0c0e15d80e382073c05657b2d57f52611c324599508cdb62b9debb8
readonly compose_sha=e0b321eab9efea38d92f511ebb259f8b6f5604fffcb40df86871c799328fb520

fail() { echo "GLM 6K-prefill rollout: $*" >&2; exit 2; }

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this rollout may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] || fail "invalid experiment directory"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/node06-prefill-6144-rollout.sh" ]] || fail "execute the staged rollout authority"
[[ $(sha256sum "$compose" | awk '{print $1}') == "$compose_sha" ]] || fail "GLM Compose bytes drifted"
[[ $(docker image inspect "$fixed_image" --format '{{.Id}}') == "$fixed_image" ]] || fail "fixed image is absent"
python3 "$glm_dir/validate-compose.py"

# shellcheck disable=SC1091
source "$qwen_dir/.env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

candidate_compose() {
  GLM53_MAX_TOTAL_TOKENS=500000 GLM53_CHUNKED_PREFILL_SIZE=6144 \
    GLM53_MAX_PREFILL_TOKENS=6144 \
    docker compose -f "$compose" --project-directory "$glm_dir" "$@"
}
qwen_health() {
  printf 'header = "Authorization: Bearer %s"\n' "$VLLM_API_KEY" |
    curl -fsS --config - --max-time 5 http://127.0.0.1:8040/health >/dev/null
}
glm_health() { curl -fsS --max-time 5 http://127.0.0.1:8062/health >/dev/null; }
wait_glm() {
  local deadline=$((SECONDS + 2400)) inspect
  while ((SECONDS < deadline)); do
    if inspect=$(docker inspect "$glm" 2>/dev/null); then
      jq -e '
        length == 1 and .[0].State.Status == "running" and
        .[0].State.OOMKilled == false and .[0].RestartCount == 0
      ' <<<"$inspect" >/dev/null || return 1
    fi
    if jq -e --arg image "$fixed_image" '
      length == 1 and .[0].Image == $image and
      .[0].State.Status == "running" and .[0].State.OOMKilled == false and
      .[0].RestartCount == 0 and
      .[0].HostConfig.DeviceRequests[0].DeviceIDs == ["4","5"] and
      (.[0].Config.Cmd | any(. == "--max-total-tokens=500000")) and
      (.[0].Config.Cmd | any(. == "--chunked-prefill-size=6144")) and
      (.[0].Config.Cmd | any(. == "--max-prefill-tokens=6144"))
    ' <<<"${inspect:-[]}" >/dev/null && glm_health; then
      return 0
    fi
    sleep 5
  done
  return 1
}
glm_probe() {
  curl -fsS --max-time 120 -H 'Content-Type: application/json' \
    -d '{"model":"glm-5.3-flash","messages":[{"role":"user","content":"Reply with one short word."}],"max_tokens":8,"temperature":0}' \
    http://127.0.0.1:8062/v1/chat/completions |
    jq -e '.model == "glm-5.3-flash" and .usage.completion_tokens >= 1' >/dev/null
}
start_inference_budget() {
  [[ ${RAMJET_GPU_GUARD_RUNTIME_START_FD:-} =~ ^[0-9]+$ ]] ||
    fail "runtime-start signal is unavailable"
  printf '1' >&"$RAMJET_GPU_GUARD_RUNTIME_START_FD" ||
    fail "runtime-start signal failed"
}
record() {
  docker inspect --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}} {{json .Config.Cmd}}' "$qwen_a" "$glm"
  nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu,power.draw,power.limit --format=csv,noheader
}

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"
qwen_health || fail "Qwen A is not healthy"
[[ $(docker inspect "$glm" --format '{{.Image}}') == "$fixed_image" ]] || fail "running GLM image drifted"
predecessor=$(docker inspect "$glm")
if jq -e '.[0].State.Status == "running"' <<<"$predecessor" >/dev/null; then
  glm_health || fail "running GLM is not healthy"
  jq -e '
    .[0].Config.Cmd | any(. == "--max-total-tokens=524288") and
    any(. == "--chunked-prefill-size=4096") and
    any(. == "--max-prefill-tokens=4096")
  ' <<<"$predecessor" >/dev/null || fail "running GLM is not the admitted 4K-prefill baseline"
else
  jq -e '
    .[0].State.Status == "exited" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0 and
    (.[0].Config.Cmd | (any(. == "--max-total-tokens=524288") or
      any(. == "--max-total-tokens=500000"))) and
    (.[0].Config.Cmd | any(. == "--chunked-prefill-size=8192")) and
    (.[0].Config.Cmd | any(. == "--max-prefill-tokens=8192"))
  ' <<<"$predecessor" >/dev/null || fail "stopped predecessor is not an exact failed 8K warmup"
fi
nvidia-smi --query-gpu=index,power.limit --format=csv,noheader,nounits -i 4,5 |
  awk -F ', ' '$2 != 600 {bad=1} END {exit bad}' || fail "GLM GPUs are not at the admitted 600W inference ceiling"
qwen_a_before=$(docker inspect "$qwen_a" --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}')
record >"$experiment_dir/before.txt"
chmod 0600 "$experiment_dir/before.txt"

success=0
rollback() {
  local original_rc=$?
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  candidate_compose stop -t 5 "$glm" >"$experiment_dir/candidate-stop.txt" 2>&1
  rollback_rc=$?
  record >"$experiment_dir/final.txt" 2>&1 || rollback_rc=1
  chmod 0600 "$experiment_dir/final.txt" 2>/dev/null || true
  ((rollback_rc == 0)) || { echo "GLM 6K-prefill rollout: candidate stop failed" >&2; exit 3; }
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

started=$SECONDS
candidate_compose up -d --no-deps --force-recreate "$glm" >"$experiment_dir/recreate.txt" 2>&1
wait_glm || fail "6K-prefill GLM did not become ready"
printf '%s\n' "$((SECONDS - started))" >"$experiment_dir/readiness-seconds.txt"
start_inference_budget
glm_probe || fail "6K-prefill GLM failed its direct smoke"
qwen_health || fail "Qwen A stopped serving"
[[ $(docker inspect "$qwen_a" --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}') == "$qwen_a_before" ]] || fail "Qwen A identity changed"
record >"$experiment_dir/ready.txt"
chmod 0600 "$experiment_dir/ready.txt"
success=1
echo "GLM 6K-prefill candidate is ready; Qwen A is unchanged"
