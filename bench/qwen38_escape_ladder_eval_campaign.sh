#!/usr/bin/env bash
# Escape-ladder steering evaluation on node06 engine B (guarded window).
# NEVER touches the shared load balancer: the candidate compose isolates
# engine B on its own network, so the LB routes around it while A keeps
# serving. All experiment traffic goes straight to 127.0.0.1:8041.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
baseline_image='vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'
ladder_reps=${QWEN38_LADDER_REPS:-1}

fail() {
  echo "qwen escape ladder evaluation: $*" >&2
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
[[ $runner == "$experiment_dir/qwen38_escape_ladder_eval_campaign.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] ||
  fail "canonical Compose bytes drifted"
[[ $ladder_reps =~ ^[1-3]$ ]] || fail "ladder repetitions must be 1, 2, or 3"

candidate_compose=$experiment_dir/docker-compose.steer.yaml
grep -q "steer_isolated" "$candidate_compose" ||
  fail "candidate Compose must be rendered with --isolate (never commandeer the shared LB)"
plugin_image_id=$(cat "$experiment_dir/image-id.txt")
for artifact in ladder_probe.py ladder_cases.json steering-vector.safetensors \
  "$candidate_compose" "$experiment_dir/image-id.txt" \
  "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  "$experiment_dir/capture_node06.sh"; do
  [[ -f $artifact ]] || fail "missing staged artifact: $artifact"
done
sha256sum \
  "$experiment_dir/ladder_probe.py" \
  "$experiment_dir/ladder_cases.json" \
  "$experiment_dir/steering-vector.safetensors" \
  "$candidate_compose" "$experiment_dir/image-id.txt" \
  "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  "$experiment_dir/capture_node06.sh" \
  >"$experiment_dir/campaign-authority.sha256"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export LLM_KEY=$VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

engine_up() {
  local file=$1
  docker compose -f "$file" --project-directory "$deployment_dir" \
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

run_ladder() {
  local output=$1
  python3 "$experiment_dir/ladder_probe.py" \
    --base-url http://127.0.0.1:8041/v1 --model "$model" \
    --cases "$experiment_dir/ladder_cases.json" \
    --reps "$ladder_reps" --out "$output"
}

engine_mutated=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  set +e
  if ((engine_mutated)); then
    engine_up "$canonical_compose" \
      >"$experiment_dir/rollback-engine.txt" 2>&1 || rollback_rc=1
    wait_engine "$baseline_image" || rollback_rc=1
    wait_lb_b_back || rollback_rc=1
  fi
  record_state "$experiment_dir/final.txt" || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "qwen escape ladder evaluation: rollback verification failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

peer_before=$(docker inspect --format \
  '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
record_state "$experiment_dir/initial.txt"

engine_mutated=1
engine_up "$candidate_compose" >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine "$plugin_image_id" || fail "steered engine did not become ready"

docker inspect "$engine" | jq -e --arg image "$plugin_image_id" '
  length == 1 and .[0].Image == $image and
  (.[0].Config.Env | any(. == "VLLM_PLUGINS=qwen38_steering")) and
  (.[0].Config.Env | any(startswith("QWEN38_STEERING_VECTOR="))) and
  (.[0].Config.Cmd | any(. == "--enforce-eager"))
' >/dev/null || fail "steering plugin is not admitted"

curl -fsS --max-time 5 http://127.0.0.1:8006/health | jq -e '
  .replicas[0].active and .replicas[0].healthy and
  (.replicas[1].healthy | not)
' >/dev/null || fail "LB must still serve peer A while isolated B is up"

run_ladder "$experiment_dir/steered-ladder.jsonl"
python3 - "$experiment_dir/steered-ladder.jsonl" <<'PY'
import json, sys, collections
tiers = collections.Counter()
for line in open(sys.argv[1]):
    if line.strip():
        tiers[json.loads(line)["tier"]] += 1
print("steered ladder tier distribution:", dict(tiers))
PY

[[ $peer_before == "$(docker inspect --format \
  '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] ||
  fail "healthy peer changed during evaluation"

printf '%s\n' "escape ladder evaluation complete; restoring production baseline"
