#!/usr/bin/env bash
# Escape-proof RESTORE window: return engine B to the exact production
# baseline (canonical Compose -> rejoins the shared network; the load
# balancer recovers it on its own and is never recreated). Peer A serves
# the entire window.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
baseline_image='vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'

fail() {
  echo "qwen escape restore: $*" >&2
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
[[ $runner == "$experiment_dir/qwen38_escape_restore_campaign.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] ||
  fail "canonical Compose bytes drifted"
for artifact in "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  "$experiment_dir/capture_node06.sh"; do
  [[ -f $artifact ]] || fail "missing staged artifact: $artifact"
done

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

engine_up() {
  docker compose -f "$canonical_compose" --project-directory "$deployment_dir" \
    up -d --no-deps --force-recreate "$engine"
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

wait_lb_b_back() {
  local deadline=$((SECONDS + 180))
  until curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null |
    jq -e '.status == "ok" and .healthy_replicas >= 2' >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 3
  done
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

engine_restored=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  set +e
  if ((engine_restored == 0)); then
    engine_up >"$experiment_dir/retry-engine.txt" 2>&1 || rollback_rc=1
    wait_engine "$baseline_image" || rollback_rc=1
  fi
  wait_lb_b_back || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "qwen escape restore: recovery retry failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

record_state "$experiment_dir/initial.txt"
engine_up >"$experiment_dir/restore-engine.txt" 2>&1
engine_restored=1
wait_engine "$baseline_image" || fail "baseline engine did not become ready"
wait_lb_b_back || fail "load balancer did not recover B on its own"
rm -f "$experiment_dir/steered-live.flag"
record_state "$experiment_dir/final.txt"
printf '%s\n' "production baseline restored; steered-live flag cleared"
