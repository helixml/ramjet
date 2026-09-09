#!/usr/bin/env bash
# Capture refusal/action prefixes and build a Qwen cyber steering bundle.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
model_revision=fc694b54fb0174e0913e6adf86691ef85a4ead47
baseline_image='vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93'
plugin_image='qwen38-steering:0.4.0'
lb_image='ghcr.io/helixml/ramjet:rust-0c7c7bc@sha256:f9215991a15a2d5ea223c84bfc4a2f7af423b0a3d4868423543c5d8a5315615f'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,http://qwen38flashnext-tp8:8000'
single_upstream='http://qwen38flashnext-a:8000'
all_profiles='standard,standard,standard'
all_kv_live='tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557'
all_kv_replay='tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558'
single_kv_live='tcp://qwen38flashnext-a:5557'
single_kv_replay='tcp://qwen38flashnext-a:5558'

fail() { echo "qwen cyber steering capture: $*" >&2; exit 2; }

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] || fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_cyber_steering_capture_campaign.sh" ]] || fail "execute the staged campaign authority"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] || fail "canonical Compose bytes drifted"

capture_dir=$experiment_dir/captures
candidate_compose=$experiment_dir/docker-compose.capture.yaml
plugin_image_id=$(cat "$experiment_dir/image-id.txt")
[[ $plugin_image_id =~ ^sha256:[0-9a-f]{64}$ ]] || fail "plugin image ID is invalid"
for artifact in qwen38_steering.py qwen38_cyber_eval.py qwen38_cyber_cases.json \
  pairs.json docker-compose.capture.yaml image-id.txt node06_gpu_guard.py \
  node06_operational_moratorium.py capture_node06.sh; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] || fail "missing staged artifact: $artifact"
done
[[ $(stat -c '%u:%a' "$capture_dir") == 0:700 ]] || fail "capture directory must be root-owned mode 0700"
[[ -z $(find "$capture_dir" -mindepth 1 -maxdepth 1 -print -quit) ]] || fail "capture directory is not empty"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export BENCH_TOKEN=$VLLM_API_KEY

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
  ((rollback_rc == 0)) || { echo "qwen cyber steering capture: rollback verification failed" >&2; exit 3; }
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

peer_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next >"$experiment_dir/preflight.txt"
record_state "$experiment_dir/initial.txt"
recreate_lb "$candidate_compose" "$single_upstream" mtp off "$single_kv_live" "$single_kv_replay" 1 1
lb_mutated=1
engine_mutated=1
compose "$candidate_compose" "$single_upstream" mtp off "$single_kv_live" "$single_kv_replay" \
  up -d --no-deps --force-recreate "$engine" >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine "$plugin_image_id" || fail "capture engine did not become ready"

python3 "$experiment_dir/qwen38_steering.py" capture \
  --base-url http://127.0.0.1:8041/v1 --model "$model" --model-revision "$model_revision" \
  --pairs "$experiment_dir/pairs.json" --capture-dir "$capture_dir" \
  --manifest "$experiment_dir/captures.json" >"$experiment_dir/capture-run.txt"

build_vector() {
  local name=$1
  shift
  docker run --rm --network none --entrypoint python3 \
    -v "$experiment_dir:/experiment" "$plugin_image" \
    /experiment/qwen38_steering.py build \
    --manifest /experiment/captures.json --capture-dir /experiment/captures \
    --include-split train --output "/experiment/$name.safetensors" \
    --summary "/experiment/$name-summary.json" "$@" >"$experiment_dir/$name-build.txt"
}
build_vector mean
build_vector mean-ortho --orthogonalize-control-mean
build_vector pairnorm --pair-normalize
build_vector pairnorm-ortho --pair-normalize --orthogonalize-control-mean
docker run --rm --network none --entrypoint python3 \
  -v "$experiment_dir:/experiment" "$plugin_image" \
  /experiment/qwen38_steering.py bundle \
  --vector mean=/experiment/mean.safetensors \
  --vector mean-ortho=/experiment/mean-ortho.safetensors \
  --vector pairnorm=/experiment/pairnorm.safetensors \
  --vector pairnorm-ortho=/experiment/pairnorm-ortho.safetensors \
  --output /experiment/cyber-refusal-bundle.safetensors \
  --summary /experiment/cyber-refusal-bundle-summary.json >"$experiment_dir/bundle-build.txt"

[[ $peer_before == "$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] || fail "healthy peer changed during capture"
printf '%s\n' "capture and bundle build complete; restoring exact production baseline"
