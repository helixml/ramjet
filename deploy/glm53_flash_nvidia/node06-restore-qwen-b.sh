#!/usr/bin/env bash
# Stop the isolated GLM canary and restore canonical Qwen engine B without
# recreating the shared load balancer. Run under node06_gpu_guard.py so model
# reload and the serving proof remain thermally bounded.
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

fail() {
  echo "GLM node06 restore: $*" >&2
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
[[ $runner == "$experiment_dir/node06-restore-qwen-b.sh" ]] ||
  fail "execute the staged restore authority"
[[ $(sha256sum "$qwen_compose" | awk '{print $1}') == "$EXPECTED_QWEN_COMPOSE_SHA256" ]] ||
  fail "operational Qwen Compose bytes drifted"

# shellcheck disable=SC1091
source "$qwen_dir/.env"
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

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

serve_probe() {
  curl -fsS --max-time 60 -H "Authorization: Bearer $VLLM_API_KEY" \
    -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg model "$qwen_model" \
      '{model:$model,messages:[{role:"user",content:"Reply with one short word."}],max_tokens:4,temperature:0}')" \
    http://127.0.0.1:8006/v1/chat/completions >/dev/null
}

qwen_a_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a")
direct_health 8040 || fail "Qwen A is not healthy"
docker compose -f "$glm_compose" --project-directory "$glm_dir" stop -t 120 "$glm_b"
docker compose -f "$qwen_compose" --project-directory "$qwen_dir" \
  up -d --no-deps --force-recreate "$qwen_b"
wait_qwen_b || fail "canonical Qwen B did not become ready"
wait_lb_full || fail "unchanged load balancer did not recover Qwen B"
serve_probe || fail "restored Qwen stack did not serve"
[[ $(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$qwen_a") == "$qwen_a_before" ]] ||
  fail "Qwen A identity changed during restore"
rm -f "$experiment_dir/glm-live.flag"
printf '%s\n' "Qwen B restored from exact canonical Compose; shared LB recovered without a recreate"
