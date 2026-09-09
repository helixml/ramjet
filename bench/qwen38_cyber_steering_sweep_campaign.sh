#!/usr/bin/env bash
# DEPRECATED PATTERN (2026-09-09, two node06 LB outages): this script
# recreates the SHARED production load balancer to single-home an engine.
# Do not run it as-is; use the qwen38_escape_* / cyber capture pattern instead:
# render the candidate compose with --isolate, never touch ds4-loadbalancer,
# and hit the experiment engine directly on its host port.
# Search Qwen cyber steering direction/layer/scale on one warm TP4 replica.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=9dc3e797bee511d5f3b6bb6022c47471db7c054885c1141f4f982bd270c9a847
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
baseline_image='vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
lb_image='ghcr.io/helixml/ramjet:rust-ff8a4af@sha256:e4d71dbbe7050b336dbc1ff6ad28c3f2235ee963f29f4524cf8ed075dbbeb5b0'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,http://qwen38flashnext-tp8:8000'
single_upstream='http://qwen38flashnext-a:8000'
all_profiles='standard,standard,standard'
all_kv_live='tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557'
all_kv_replay='tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558'
single_kv_live='tcp://qwen38flashnext-a:5557'
single_kv_replay='tcp://qwen38flashnext-a:5558'

fail() { echo "qwen cyber steering sweep: $*" >&2; exit 2; }

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] || fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_cyber_steering_sweep_campaign.sh" ]] || fail "execute the staged campaign authority"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] || fail "canonical Compose bytes drifted"

candidate_compose=$experiment_dir/docker-compose.sweep.yaml
control_file=$experiment_dir/control.json
plugin_image_id=$(cat "$experiment_dir/image-id.txt")
[[ $plugin_image_id =~ ^sha256:[0-9a-f]{64}$ ]] || fail "plugin image ID is invalid"
for artifact in qwen38_cyber_eval.py qwen38_cyber_cases.json cyber-refusal-bundle.safetensors \
  control.json docker-compose.sweep.yaml image-id.txt node06_gpu_guard.py \
  node06_operational_moratorium.py capture_node06.sh; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] || fail "missing staged artifact: $artifact"
done
[[ $(stat -c '%u:%a' "$control_file") == 0:600 ]] || fail "control file must be root-owned mode 0600"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export CYBER_EVAL_API_KEY=$VLLM_API_KEY

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
  local file=$1 upstreams=$2 profiles=$3 mode=$4 live=$5 replay=$6
  shift 6
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    RJ_ROUTE_KV_CAPACITY_TOKENS="$(capacity_for "$upstreams")" \
    RJ_ROUTE_SPECULATION_PROFILES="$profiles" RJ_ROUTE_SPECULATION_MODE="$mode" \
    RJ_KV_EVENT_LIVE_ENDPOINTS="$live" RJ_KV_EVENT_REPLAY_ENDPOINTS="$replay" \
    docker compose -f "$file" --project-directory "$deployment_dir" "$@"
}

wait_engine() {
  local expected=$1 deadline=$((SECONDS + 900)) inspect
  until inspect=$(docker inspect "$engine" 2>/dev/null) &&
    jq -e --arg image "$expected" 'length == 1 and (.[0].Image == $image or .[0].Config.Image == $image) and .[0].State.Status == "running" and .[0].State.OOMKilled == false and .[0].RestartCount == 0' <<<"$inspect" >/dev/null &&
    curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" http://127.0.0.1:8041/health >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_lb() {
  local healthy=$1 total=$2 deadline=$((SECONDS + 90)) health
  until health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null) &&
    jq -e --argjson healthy "$healthy" --argjson total "$total" '.status == "ok" and .healthy_replicas == $healthy and .active_replicas == $healthy and .total_replicas == $total' <<<"$health" >/dev/null; do
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
  local file=$1 upstreams=$2 profiles=$3 mode=$4 live=$5 replay=$6 healthy=$7 total=$8
  compose "$file" "$upstreams" "$profiles" "$mode" "$live" "$replay" \
    up -d --no-deps --force-recreate ds4-loadbalancer >"$experiment_dir/lb-$healthy-recreate.txt" 2>&1
  wait_lb "$healthy" "$total"
}

record_state() {
  local output=$1
  {
    date -u +%FT%TZ
    docker inspect --format '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' "$peer" "$engine"
    curl -fsS --max-time 5 http://127.0.0.1:8006/health
  } >"$output"
}

lb_mutated=0
engine_mutated=0
rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  set +e
  if ((engine_mutated)); then
    compose "$canonical_compose" "$single_upstream" mtp off "$single_kv_live" "$single_kv_replay" \
      up -d --no-deps --force-recreate "$engine" >"$experiment_dir/rollback-engine.txt" 2>&1 || rollback_rc=1
    wait_engine "$baseline_image" || rollback_rc=1
  fi
  if ((lb_mutated)); then
    recreate_lb "$canonical_compose" "$all_upstreams" "$all_profiles" off "$all_kv_live" "$all_kv_replay" 2 3 || rollback_rc=1
  fi
  record_state "$experiment_dir/final.txt" || rollback_rc=1
  ((rollback_rc == 0)) || { echo "qwen cyber steering sweep: rollback verification failed" >&2; exit 3; }
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

run_eval() {
  local output=$1
  shift
  python3 "$experiment_dir/qwen38_cyber_eval.py" run \
    --base-url http://127.0.0.1:8041/v1 --model "$model" \
    --cases "$experiment_dir/qwen38_cyber_cases.json" --concurrency 8 \
    --max-tokens 96 --report-policy-failures --output "$experiment_dir/$output.json" \
    "$@" >"$experiment_dir/$output.jsonl"
}

generation=0
sweep_index=$experiment_dir/sweep-index.jsonl
: >"$sweep_index"
chmod 0600 "$sweep_index"
eval_config() {
  local phase=$1 direction=$2 scale=$3 layers=$4 label=$5
  generation=$((generation + 1))
  python3 "$experiment_dir/qwen38_cyber_eval.py" set-control \
    --output "$control_file" --generation "$generation" \
    --direction-index "$direction" --scale "$scale" --layers "$layers"
  run_eval "$label" --split validation --split safety
  jq -nc --arg phase "$phase" --arg label "$label" --arg layers "$layers" \
    --argjson direction "$direction" --argjson scale "$scale" \
    --argjson authorized "$(jq '.summary.authorized_tool_calls' "$experiment_dir/$label.json")" \
    --argjson schema_valid "$(jq '.summary.authorized_schema_valid' "$experiment_dir/$label.json")" \
    --argjson boundary_passed "$(jq '.summary.boundary_passed' "$experiment_dir/$label.json")" \
    --argjson unsafe "$(jq '.summary.unsafe_tool_ids|length' "$experiment_dir/$label.json")" \
    --argjson malformed "$(jq '.summary.malformed' "$experiment_dir/$label.json")" \
    '{phase:$phase,label:$label,direction:$direction,scale:$scale,layers:$layers,authorized:$authorized,schema_valid:$schema_valid,boundary_passed:$boundary_passed,unsafe:$unsafe,malformed:$malformed}' \
    >>"$sweep_index"
}

peer_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next >"$experiment_dir/preflight.txt"
record_state "$experiment_dir/initial.txt"
recreate_lb "$candidate_compose" "$single_upstream" mtp off "$single_kv_live" "$single_kv_replay" 1 1
lb_mutated=1
run_eval baseline-compiled --split validation --split test --split safety

engine_mutated=1
compose "$candidate_compose" "$single_upstream" mtp off "$single_kv_live" "$single_kv_replay" \
  up -d --no-deps --force-recreate "$engine" >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine "$plugin_image_id" || fail "sweep engine did not become ready"
docker inspect "$engine" | jq -e --arg image "$plugin_image_id" 'length == 1 and .[0].Image == $image and (.[0].Config.Env|any(.=="VLLM_PLUGINS=qwen38_steering")) and (.[0].Config.Env|any(startswith("QWEN38_STEERING_CONTROL_FILE="))) and (.[0].Config.Cmd|any(.=="--enforce-eager"))' >/dev/null || fail "dynamic steering plugin is not admitted"

python3 "$experiment_dir/qwen38_cyber_eval.py" set-control --output "$control_file" \
  --generation 1 --direction-index 0 --scale 0 --layers all
generation=1
run_eval baseline-zero-selection --split validation --split safety
run_eval baseline-zero-final --split test --split safety

for direction in 0 1 2 3; do
  eval_config direction "$direction" 1 32-35 "direction-d${direction}-s1-l32-35"
done
best_direction=$(jq -sr 'map(select(.phase=="direction" and .unsafe==0 and .malformed==0)) | max_by([.authorized,.boundary_passed,.schema_valid,(-.direction)]) | .direction' "$sweep_index")
[[ $best_direction =~ ^[0-3]$ ]] || fail "no safe direction candidate"

for layers in 20-23 28-35 32-35; do
  for scale in 0.5 1 1.5 2; do
    safe_scale=${scale//./p}
    eval_config grid "$best_direction" "$scale" "$layers" "grid-d${best_direction}-s${safe_scale}-l${layers}"
  done
done

jq -s 'map(select(.phase=="grid" and .unsafe==0 and .malformed==0)) | max_by([.authorized,.boundary_passed,.schema_valid,(-.scale)])' \
  "$sweep_index" >"$experiment_dir/selected.json"
chmod 0600 "$experiment_dir/selected.json"
selected_direction=$(jq -r .direction "$experiment_dir/selected.json")
selected_scale=$(jq -r .scale "$experiment_dir/selected.json")
selected_layers=$(jq -r .layers "$experiment_dir/selected.json")
[[ $selected_direction =~ ^[0-3]$ && $selected_layers =~ ^[0-9]+-[0-9]+$ ]] || fail "no safe grid candidate"
generation=$((generation + 1))
python3 "$experiment_dir/qwen38_cyber_eval.py" set-control --output "$control_file" \
  --generation "$generation" --direction-index "$selected_direction" \
  --scale "$selected_scale" --layers "$selected_layers"
run_eval selected-final-1 --split test --split safety
run_eval selected-final-2 --split test --split safety
python3 "$experiment_dir/qwen38_cyber_eval.py" compare \
  --baseline "$experiment_dir/baseline-zero-final.json" \
  --candidate "$experiment_dir/selected-final-1.json" \
  --output "$experiment_dir/comparison-final-1.json" >"$experiment_dir/comparison-final-1.txt"
python3 "$experiment_dir/qwen38_cyber_eval.py" compare \
  --baseline "$experiment_dir/baseline-zero-final.json" \
  --candidate "$experiment_dir/selected-final-2.json" \
  --output "$experiment_dir/comparison-final-2.json" >"$experiment_dir/comparison-final-2.txt"

[[ $peer_before == "$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] || fail "healthy peer changed during sweep"
printf '%s\n' "sweep complete; restoring exact production baseline"
