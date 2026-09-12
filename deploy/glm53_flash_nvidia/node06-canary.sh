#!/usr/bin/env bash
# Activate one LB-isolated GLM-5.3-Flash canary on node06 engine B.
#
# Run this only as a child of bench/node06_gpu_guard.py. The shared load
# balancer is never recreated: canonical Qwen B is stopped, so the unchanged
# LB keeps serving through Qwen A, while the differently named GLM service is
# reachable only through its loopback host port. Any failure restores Qwen B.
set -Eeuo pipefail

readonly qwen_dir=/home/luke/inference/qwen38_flash_next
readonly qwen_compose="$qwen_dir/docker-compose.yaml"
readonly glm_dir=/home/luke/inference/glm53_flash_nvidia
readonly glm_compose="$glm_dir/docker-compose.yaml"
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
readonly qwen_a=qwen38flashnext-a
readonly qwen_b=qwen38flashnext-b
readonly glm_b=glm53nvidia-b
readonly qwen_model=qwen3.8-flash-next
readonly qwen_image='sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'
readonly glm_image='sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'

fail() {
  echo "GLM node06 canary: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
: "${EXPECTED_QWEN_COMPOSE_SHA256:?set exact operational Qwen Compose SHA-256}"

experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$glm_dir/.experiments/"* ]] ||
  fail "experiment directory is outside the GLM deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] ||
  fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/node06-canary.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$qwen_compose" | awk '{print $1}') == "$EXPECTED_QWEN_COMPOSE_SHA256" ]] ||
  fail "operational Qwen Compose bytes drifted"
[[ $(sha256sum "$glm_compose" | awk '{print $1}') == 46f868e0aca5ad2ebcb45ee5050cc38427b255e33f62553b52f9754433015cce ]] ||
  fail "operational GLM Compose bytes drifted"

# shellcheck disable=SC1091
source "$qwen_dir/.env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

qwen_compose_cmd() {
  docker compose -f "$qwen_compose" --project-directory "$qwen_dir" "$@"
}

glm_compose_cmd() {
  docker compose -f "$glm_compose" --project-directory "$glm_dir" "$@"
}

direct_health() {
  local port=$1
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    "http://127.0.0.1:${port}/health" >/dev/null
}

wait_qwen_b() {
  local deadline=$((SECONDS + 900)) inspect
  until inspect=$(docker inspect "$qwen_b" 2>/dev/null) &&
    jq -e --arg image "$qwen_image" '
      length == 1 and .[0].Image == $image and
      .[0].State.Status == "running" and .[0].State.OOMKilled == false and
      .[0].RestartCount == 0
    ' <<<"$inspect" >/dev/null && direct_health 8041; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_glm_b() {
  local deadline=$((SECONDS + 900)) inspect
  while ((SECONDS < deadline)); do
    if inspect=$(docker inspect "$glm_b" 2>/dev/null); then
      if jq -e --arg image "$glm_image" '
        length == 1 and .[0].Image == $image and
        .[0].State.Status == "running" and .[0].State.OOMKilled == false and
        .[0].RestartCount == 0 and
        (.[0].HostConfig.DeviceRequests[0].DeviceIDs == ["4","5","6","7"]) and
        (.[0].Config.Cmd | any(. == "--quantization=modelopt_fp4")) and
        (.[0].Config.Cmd | any(. == "--kv-cache-dtype=fp8")) and
        (.[0].Config.Cmd | any(. == "--max-num-seqs=4"))
      ' <<<"$inspect" >/dev/null && direct_health 8061; then
        return 0
      fi
      jq -e '.[0].State.Status == "exited" or .[0].State.Status == "dead"' \
        <<<"$inspect" >/dev/null && return 1
    fi
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
  return 1
}

wait_lb_a_only() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
    awk '
      /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-a:8000"\} 1$/ {a=1}
      /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-b:8000"\} 0$/ {b=1}
      END {exit !(a && b)}
    '; do
    ((SECONDS < deadline)) || return 1
    sleep 3
  done
}

wait_lb_full() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
    awk '
      /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-a:8000"\} 1$/ {a=1}
      /^ramjet_upstream_up\{upstream="http:\/\/qwen38flashnext-b:8000"\} 1$/ {b=1}
      END {exit !(a && b)}
    '; do
    ((SECONDS < deadline)) || return 1
    sleep 3
  done
}

wait_qwen_b_idle() {
  local deadline=$((SECONDS + 300))
  until curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
    grep -Fx 'ramjet_upstream_inflight{upstream="http://qwen38flashnext-b:8000"} 0' >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

b_gpus_are_free() {
  local allowed uuid pid
  allowed=$(nvidia-smi --query-gpu=index,uuid --format=csv,noheader,nounits |
    awk -F ', ' '$1 >= 4 {print $2}')
  while IFS=', ' read -r uuid pid; do
    [[ -z $uuid ]] && continue
    if grep -Fxq "$uuid" <<<"$allowed"; then
      echo "GPU assigned to candidate B is still owned by compute PID $pid" >&2
      return 1
    fi
  done < <(nvidia-smi --query-compute-apps=gpu_uuid,pid --format=csv,noheader,nounits)
}

serve_probe() {
  curl -fsS --max-time 60 -H "Authorization: Bearer $VLLM_API_KEY" \
    -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg model "$qwen_model" \
      '{model:$model,messages:[{role:"user",content:"Reply with one short word."}],max_tokens:4,temperature:0}')" \
    http://127.0.0.1:8006/v1/chat/completions >/dev/null
}

record_state() {
  local output=$1
  {
    date -u +%FT%TZ
    for container in "$qwen_a" "$qwen_b" "$glm_b"; do
      docker inspect --format \
        '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' \
        "$container" 2>/dev/null || true
    done
    nvidia-smi --query-gpu=index,memory.used,utilization.gpu,temperature.gpu,power.draw \
      --format=csv,noheader
    curl -fsS --max-time 5 http://127.0.0.1:8006/health
  } >"$output"
}

mutated=0
success=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  ((success)) && exit 0
  set +e
  if ((mutated)); then
    glm_compose_cmd stop -t 120 "$glm_b" \
      >"$experiment_dir/rollback-glm-stop.txt" 2>&1 || rollback_rc=1
    qwen_compose_cmd up -d --no-deps --force-recreate "$qwen_b" \
      >"$experiment_dir/rollback-qwen-start.txt" 2>&1 || rollback_rc=1
    wait_qwen_b || rollback_rc=1
    wait_lb_full || rollback_rc=1
  fi
  record_state "$experiment_dir/final.txt" || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "GLM node06 canary: rollback verification failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

python3 "$glm_dir/validate-compose.py"
python3 "$glm_dir/verify-model.py" \
  /prod/models/nvidia/GLM-5.3-Flash-NVFP4-423acf37583782c51c142d145aef733d72943d93
direct_health 8040 || fail "Qwen A is not healthy"
docker inspect "$qwen_a" "$qwen_b" | jq -e --arg image "$qwen_image" '
  length == 2 and ([.[].Image] | all(. == $image)) and
  ([.[].State.Status] | all(. == "running")) and
  ([.[].State.OOMKilled] | all(. == false)) and
  ([.[].RestartCount] | all(. == 0))
' >/dev/null || fail "Qwen A/B baseline identity is not admitted"
record_state "$experiment_dir/initial.txt"
qwen_a_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a")
wait_qwen_b_idle || fail "Qwen B did not drain production work before withdrawal"

mutated=1
qwen_compose_cmd stop -t 120 "$qwen_b" >"$experiment_dir/qwen-b-stop.txt" 2>&1
wait_lb_a_only || fail "shared LB did not isolate stopped Qwen B"
serve_probe || fail "Qwen A did not serve through the unchanged LB"
b_gpus_are_free || fail "candidate GPUs 4-7 are not free after stopping Qwen B"

started=$SECONDS
glm_compose_cmd up -d --no-deps --force-recreate "$glm_b" \
  >"$experiment_dir/glm-b-start.txt" 2>&1
wait_glm_b || fail "GLM B did not become ready"
printf '%s\n' "$((SECONDS - started))" >"$experiment_dir/glm-readiness-seconds.txt"
wait_lb_a_only || fail "GLM B became visible to the shared LB"
serve_probe || fail "Qwen A stopped serving while GLM B was isolated"

[[ $(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a") == "$qwen_a_before" ]] ||
  fail "Qwen A identity changed during the canary"
docker logs "$glm_b" 2>&1 |
  grep -Ei 'loading model weights|model loading|cache_config_info|GPU KV cache size|Mamba cache|graph|JIT|CUDA|NCCL|Xid|OOM|traceback|fatal' |
  tail -n 400 >"$experiment_dir/glm-runtime-markers.txt" || true
record_state "$experiment_dir/ready.txt"
printf '%s\n' "$(date -u +%FT%TZ) glm-live engine=$glm_b port=8061 isolated=true qwen-a-serving=true" \
  >"$experiment_dir/glm-live.flag"
success=1
printf '%s\n' "GLM B ready and LB-isolated on :8061; Qwen A is unchanged and serving"
