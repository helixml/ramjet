#!/usr/bin/env bash
# Guarded node06 A/B/B/A qualification for Qwen3.8-Flash-Next route-load caps.
set -euo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
compose_file=$deployment_dir/docker-compose.yaml
lock_file=/run/lock/ramjet-node06-deployment.lock
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
lb_image_id='sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000'
metrics_urls='http://127.0.0.1:8040/metrics,http://127.0.0.1:8041/metrics'
model=qwen3.8-flash-next
engines=(qwen38flashnext-a qwen38flashnext-b)
mixed_bench_sha=4471df918d075236016633ad5d0f4fc8e88531ab1853b7158e568c72a8492ee5
engine_metrics_sha=67e26a0d7e548fc8e8d193dc332f13962a4e7dc6775f8e255da37c9508ce8ce4
gpu_guard_sha=91853921fbe01d4eaf1d6b7a15921e4d3c991828afe156240e3690c7bf23dcd8
moratorium_sha=cc778fc2252567843c1e2b0b8cbbd207102294debb6ab8b641ce7836bc9f38a1

fail() {
  echo "qwen38 route-load cap A/B: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
experiment_dir=$(realpath -e -- "$1")
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] || \
  fail "experiment directory is outside the Qwen deployment"
[[ -d $experiment_dir && ! -L $experiment_dir ]] || \
  fail "experiment directory is not a real directory"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] || \
  fail "experiment directory must be root-owned mode 0700"
for artifact in mixed_bench.py engine_metrics.py node06_gpu_guard.py \
  node06_operational_moratorium.py; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] || \
    fail "missing experiment artifact: $artifact"
done
[[ $(sha256sum "$experiment_dir/mixed_bench.py" | awk '{print $1}') == "$mixed_bench_sha" ]] || \
  fail "mixed benchmark bytes do not match the merged authority"
[[ $(sha256sum "$experiment_dir/engine_metrics.py" | awk '{print $1}') == "$engine_metrics_sha" ]] || \
  fail "engine metrics bytes do not match the merged authority"
[[ $(sha256sum "$experiment_dir/node06_gpu_guard.py" | awk '{print $1}') == "$gpu_guard_sha" ]] || \
  fail "GPU guard bytes do not match the qualified authority"
[[ $(sha256sum "$experiment_dir/node06_operational_moratorium.py" | awk '{print $1}') == "$moratorium_sha" ]] || \
  fail "operational policy bytes do not match the qualified authority"
[[ -r $compose_file && ! -L $compose_file ]] || fail "canonical Compose file is unavailable"

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

engine_state() {
  docker inspect --format \
    '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}}' \
    "${engines[@]}"
}

require_engines_unchanged() {
  local observed=$1
  engine_state >"$observed"
  cmp -s "$experiment_dir/engines.before.txt" "$observed" || \
    fail "an engine changed during the LB-only campaign"
}

check_lb() {
  local expected_cap=$1 health up_count up_sum
  [[ $(docker inspect --format '{{.Config.Image}}' ds4-loadbalancer) == "$lb_image" ]] || \
    return 1
  [[ $(docker inspect --format '{{.Image}}' ds4-loadbalancer) == "$lb_image_id" ]] || \
    return 1
  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ds4-loadbalancer |
    grep -Fx "RJ_ROUTE_MAX_LOAD_UNITS=$expected_cap" >/dev/null || \
    return 1
  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ds4-loadbalancer |
    grep -Fx 'RJ_ROUTE_PHASE_AWARE_LOAD=true' >/dev/null || \
    return 1
  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' ds4-loadbalancer |
    grep -Fx "RJ_UPSTREAM=$upstreams" >/dev/null || return 1
  health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health) || return 1
  jq -e '.status == "ok" and .healthy_replicas == 2 and .total_replicas == 2' \
    <<<"$health" >/dev/null || return 1
  read -r up_count up_sum < <(
    curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
      awk '$1 ~ /^ramjet_upstream_up([{]|$)/ {count++; sum += $NF} END {print count+0, sum+0}'
  ) || return 1
  [[ $up_count == 2 && $up_sum == 2 ]]
}

require_lb() {
  check_lb "$1" || fail "load balancer is not exact and healthy 2/2 at cap $1"
}

wait_for_lb() {
  local expected_cap=$1 deadline=$((SECONDS + 60))
  until check_lb "$expected_cap" 2>/dev/null; do
    ((SECONDS < deadline)) || fail "load balancer did not become healthy at cap $expected_cap"
    sleep 1
  done
  require_lb "$expected_cap"
}

mutated=0
rollback() {
  local status=$?
  trap - EXIT INT TERM HUP
  if ((mutated)); then
    cd "$deployment_dir"
    env LB_IMAGE="$lb_image" RJ_ROUTE_MAX_LOAD_UNITS=8 \
      docker compose -f "$compose_file" up -d --no-deps --force-recreate \
        ds4-loadbalancer >"$experiment_dir/rollback.txt" 2>&1 || status=1
    wait_for_lb 8 || status=1
  fi
  exit "$status"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

render_and_recreate() {
  local cap=$1 label=$2 render=$experiment_dir/$label-render.json
  cd "$deployment_dir"
  env LB_IMAGE="$lb_image" RJ_ROUTE_MAX_LOAD_UNITS="$cap" \
    docker compose -f "$compose_file" config --format json >"$render"
  jq -e --arg image "$lb_image" --arg cap "$cap" --arg upstreams "$upstreams" '
    .services["ds4-loadbalancer"] |
    .image == $image and
    .environment.RJ_ROUTE_MAX_LOAD_UNITS == $cap and
    .environment.RJ_ROUTE_PHASE_AWARE_LOAD == "true" and
    .environment.RJ_UPSTREAM == $upstreams
  ' "$render" >/dev/null || fail "render authority failed for $label"
  mutated=1
  env LB_IMAGE="$lb_image" RJ_ROUTE_MAX_LOAD_UNITS="$cap" \
    docker compose -f "$compose_file" up -d --no-deps --force-recreate \
      ds4-loadbalancer >"$experiment_dir/$label-recreate.txt" 2>&1
  wait_for_lb "$cap"
  require_engines_unchanged "$experiment_dir/$label-engines.txt"
}

run_cell() {
  local label=$1 cap=$2 prefill=$3 decoders=$4 decode=$5 runs=$6 lead_ms=$7
  local started result thermal guard_stdout
  result=$experiment_dir/$label.json
  thermal=$experiment_dir/$label-thermal.jsonl
  guard_stdout=$experiment_dir/$label-guard.stdout
  [[ ! -e $result && ! -e $thermal && ! -e $guard_stdout ]] || \
    fail "$label evidence already exists"
  started=$(date -u +%FT%TZ)
  python3 "$experiment_dir/node06_gpu_guard.py" \
    --output "$thermal" \
    --label "qwen38-$label" \
    --max-runtime-seconds 1500 \
    -- \
    env \
      BENCH_TOKEN="${VLLM_API_KEY:-qwen-local}" \
      METRICS_URLS="$metrics_urls" \
      BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
      MIXED_ORDER=prefill-first \
      MIXED_LEAD_MS="$lead_ms" \
      SALT="$label-$(date -u +%s%N)" \
      SWEEP_LABEL="$label" \
      bash -c 'exec python3 "$1" "$2" "$3" "$4" "$5" "$6" "$7" >"$8"' \
        _ "$experiment_dir/mixed_bench.py" http://127.0.0.1:8006 "$model" \
        "$prefill" "$decoders" "$decode" "$runs" "$result" \
    >"$guard_stdout"
  jq -e '
    (.errors | not) and
    (.measurement_error | not) and
    .reconciliation.reconciled == true and
    .reconciliation.matches.requests == true and
    .reconciliation.matches.prompt_tokens == true and
    .reconciliation.matches.generation_tokens == true and
    .reconciliation.matches.preemptions == true and
    .reconciliation.speculation_match == true
  ' "$result" >/dev/null || fail "$label did not reconcile"
  jq -e 'select(.type == "final") | .status == "passed"' "$thermal" >/dev/null || \
    fail "$label thermal guard did not pass"
  if [[ $cap == 32 && $decoders == 16 ]]; then
    jq -e 'all(.run_route_relationships[];
      .decoder_unknown_route == 0 and .decoder_other_route >= 14)' \
      "$result" >/dev/null || fail "$label did not separate decoders from the active prefill"
  fi
  for engine in "${engines[@]}"; do
    if docker logs --since "$started" "$engine" 2>&1 |
      grep -Eiq 'CUDA.*error|NCCL.*error|out of memory|Xid|Traceback|EngineCore.*fail|JIT.*(fail|error)'; then
      fail "$label produced a new engine failure marker"
    fi
  done
  require_lb "$cap"
  require_engines_unchanged "$experiment_dir/$label-engines-after.txt"
}

engine_state >"$experiment_dir/engines.before.txt"
require_lb 8

# Prove the exact merged harness before the first load-balancer mutation.
run_cell smoke 8 32000 2 64 1 200

render_and_recreate 8 cap8-a1
run_cell cap8-a1 8 128000 16 512 3 200
render_and_recreate 32 cap32-b1
run_cell cap32-b1 32 128000 16 512 3 200
render_and_recreate 32 cap32-b2
run_cell cap32-b2 32 128000 16 512 3 200
render_and_recreate 8 cap8-a2
run_cell cap8-a2 8 128000 16 512 3 200

require_lb 8
require_engines_unchanged "$experiment_dir/engines.after.txt"
mutated=0
trap - EXIT INT TERM HUP
printf '%s\n' 'qwen38 route-load cap A/B completed at cap 8'
