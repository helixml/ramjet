#!/usr/bin/env bash
# Guarded node06 A/B/B/A qualification for Qwen3.8-Flash-Next route-load caps.
set -euo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
compose_file=$deployment_dir/docker-compose.yaml
compose_sha=5cfcf93f9cb4ca552d3ae91cdb0b26b2740e43ac2768123ea014a4d197fb6e1a
lock_file=/run/lock/ramjet-node06-deployment.lock
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
lb_image_id='sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000'
metrics_urls='http://127.0.0.1:8040/metrics,http://127.0.0.1:8041/metrics'
model=qwen3.8-flash-next
engines=(qwen38flashnext-a qwen38flashnext-b)
compose_project=qwen38_flash_next
mixed_bench_sha=4471df918d075236016633ad5d0f4fc8e88531ab1853b7158e568c72a8492ee5
engine_metrics_sha=67e26a0d7e548fc8e8d193dc332f13962a4e7dc6775f8e255da37c9508ce8ce4
gpu_guard_sha=91853921fbe01d4eaf1d6b7a15921e4d3c991828afe156240e3690c7bf23dcd8
moratorium_sha=cc778fc2252567843c1e2b0b8cbbd207102294debb6ab8b641ce7836bc9f38a1
capture_sha=c8cb063ebd8e09d3e4732391986371298b2ad383105cdb99d1f1b6ba86463a3c
compose_timeout_seconds=60
smoke_max_seconds=120
full_max_seconds=300
campaign_max_seconds=1800

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
expected_entries=$'capture_node06.sh\nengine_metrics.py\nmixed_bench.py\nnode06_gpu_guard.py\nnode06_operational_moratorium.py\nqwen38_route_load_cap_abba.sh'
observed_entries=$(find "$experiment_dir" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort)
[[ $observed_entries == "$expected_entries" ]] || \
  fail "experiment directory must contain only the staged authorities"
for artifact in mixed_bench.py engine_metrics.py node06_gpu_guard.py \
  node06_operational_moratorium.py capture_node06.sh; do
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
[[ $(sha256sum "$experiment_dir/capture_node06.sh" | awk '{print $1}') == "$capture_sha" ]] || \
  fail "capture bytes do not match the qualified authority"
[[ -r $compose_file && ! -L $compose_file ]] || fail "canonical Compose file is unavailable"
[[ $(sha256sum "$compose_file" | awk '{print $1}') == "$compose_sha" ]] || \
  fail "canonical Compose bytes drifted"
# All later evidence creation is exclusive. A rerun must use a fresh directory.
set -o noclobber

mapfile -t bearer_headers < <(
  grep -Eo 'Bearer [A-Za-z0-9_-]+' /etc/caddy/Caddyfile
)
[[ ${#bearer_headers[@]} == 1 ]] || fail "Caddy bearer authority is not singular"
bench_token=${bearer_headers[0]#Bearer }
[[ ${#bench_token} -ge 16 ]] || fail "Caddy bearer authority is invalid"
export BENCH_TOKEN=$bench_token
compose_environment=(
  env
  LB_IMAGE="$lb_image"
  RJ_UPSTREAM="$upstreams"
  RJ_AFFINITY=prefix
  RJ_ROUTE_ALPHA=4
  RJ_ROUTE_CHUNK_BYTES=2048
  RJ_ROUTE_MAX_PREFIX_BYTES=2097152
  RJ_ROUTE_MAX_OVERLAP_BLOCKS=32
  RJ_ROUTE_LOAD_UNIT_BYTES=32768
  RJ_ROUTE_PHASE_AWARE_LOAD=true
  RJ_ROUTE_JOURNAL=true
)

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
  local expected_cap=$1 health up_count up_sum config_files project live_environment
  [[ $(docker inspect --format '{{.Config.Image}}' ds4-loadbalancer) == "$lb_image" ]] || \
    return 1
  [[ $(docker inspect --format '{{.Image}}' ds4-loadbalancer) == "$lb_image_id" ]] || \
    return 1
  config_files=$(docker inspect --format \
    '{{index .Config.Labels "com.docker.compose.project.config_files"}}' \
    ds4-loadbalancer) || return 1
  [[ $config_files == "$compose_file" ]] || return 1
  project=$(docker inspect --format \
    '{{index .Config.Labels "com.docker.compose.project"}}' \
    ds4-loadbalancer) || return 1
  [[ $project == "$compose_project" ]] || return 1
  live_environment=$(docker inspect --format \
    '{{range .Config.Env}}{{println .}}{{end}}' ds4-loadbalancer) || return 1
  for expected in \
    "RJ_UPSTREAM=$upstreams" \
    RJ_AFFINITY=prefix \
    RJ_ROUTE_ALPHA=4 \
    RJ_ROUTE_CHUNK_BYTES=2048 \
    RJ_ROUTE_MAX_PREFIX_BYTES=2097152 \
    RJ_ROUTE_MAX_OVERLAP_BLOCKS=32 \
    RJ_ROUTE_LOAD_UNIT_BYTES=32768 \
    "RJ_ROUTE_MAX_LOAD_UNITS=$expected_cap" \
    RJ_ROUTE_PHASE_AWARE_LOAD=true \
    RJ_ROUTE_JOURNAL=true; do
    grep -Fx "$expected" <<<"$live_environment" >/dev/null || return 1
  done
  health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health) || return 1
  jq -e '.status == "ok" and .healthy_replicas == 2 and .total_replicas == 2' \
    <<<"$health" >/dev/null || return 1
  read -r up_count up_sum < <(
    curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
      awk '$1 ~ /^ramjet_upstream_up([{]|$)/ {count++; sum += $NF} END {print count+0, sum+0}'
  ) || return 1
  [[ $up_count == 2 && $up_sum == 2 ]]
}

check_idle() {
  local values inflight_count inflight_sum load_count load_sum
  values=$(
    curl -fsS --max-time 5 http://127.0.0.1:8007/metrics |
      awk '
        $1 ~ /^ramjet_upstream_inflight([{]|$)/ {inflight_count++; inflight_sum += $NF}
        $1 ~ /^ramjet_upstream_load_units([{]|$)/ {load_count++; load_sum += $NF}
        END {print inflight_count+0, inflight_sum+0, load_count+0, load_sum+0}
      '
  ) || return 1
  read -r inflight_count inflight_sum load_count load_sum <<<"$values"
  [[ $inflight_count == 2 && $inflight_sum == 0 && $load_count == 2 && $load_sum == 0 ]]
}

wait_for_idle() {
  local deadline=$((SECONDS + 60))
  until check_idle; do
    ((SECONDS < deadline)) || fail "load balancer did not become idle"
    sleep 1
  done
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
    timeout --foreground "$compose_timeout_seconds" \
      "${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS=8 \
      docker compose -f "$compose_file" up -d --no-deps --force-recreate \
        ds4-loadbalancer >"$experiment_dir/rollback.txt" 2>&1 || status=1
    wait_for_lb 8 || status=1
    require_engines_unchanged "$experiment_dir/rollback-engines.txt" || status=1
  fi
  exit "$status"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

render_and_recreate() {
  local cap=$1 label=$2 proof=$experiment_dir/$label-render-proof.json
  cd "$deployment_dir"
  wait_for_idle
  # The complete Compose render contains credential-expanded environment.
  # Validate it only in-memory and persist a reviewed, non-secret projection.
  "${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS="$cap" \
    docker compose -f "$compose_file" config --format json |
    jq -e --arg image "$lb_image" --arg cap "$cap" --arg upstreams "$upstreams" '
      .services["ds4-loadbalancer"] |
      select(
        .image == $image and
        .environment.RJ_ROUTE_MAX_LOAD_UNITS == $cap and
        .environment.RJ_ROUTE_PHASE_AWARE_LOAD == "true" and
        .environment.RJ_UPSTREAM == $upstreams
      ) |
      {
        image,
        route_max_load_units: .environment.RJ_ROUTE_MAX_LOAD_UNITS,
        route_phase_aware_load: .environment.RJ_ROUTE_PHASE_AWARE_LOAD,
        upstream: .environment.RJ_UPSTREAM
      }
    ' >"$proof" || fail "render authority failed for $label"
  mutated=1
  timeout --foreground "$compose_timeout_seconds" \
    "${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS="$cap" \
    docker compose -f "$compose_file" up -d --no-deps --force-recreate \
      ds4-loadbalancer >"$experiment_dir/$label-recreate.txt" 2>&1
  wait_for_lb "$cap"
  require_engines_unchanged "$experiment_dir/$label-engines.txt"
}

prove_render_delta() {
  local proof=$experiment_dir/cap-render-parity.json
  local cap8 cap32 cap8_sha cap32_sha
  local filter='def sanitize:
    walk(
      if type == "object" then with_entries(
        if (.key | test("(?i)(token|secret|password|authorization|api[_-]?key)"))
        then .value = "<redacted>" else . end
      ) else . end
    ) |
    .services["ds4-loadbalancer"].environment.RJ_ROUTE_MAX_LOAD_UNITS = "<candidate>";
    sanitize'
  cap8=$("${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS=8 \
    docker compose -f "$compose_file" config --format json | jq -S "$filter")
  cap32=$("${compose_environment[@]}" RJ_ROUTE_MAX_LOAD_UNITS=32 \
    docker compose -f "$compose_file" config --format json | jq -S "$filter")
  cap8_sha=$(sha256sum <<<"$cap8" | awk '{print $1}')
  cap32_sha=$(sha256sum <<<"$cap32" | awk '{print $1}')
  [[ $cap8_sha == "$cap32_sha" ]] || \
    fail "cap renders differ by more than the route-load cap"
  jq -n --arg normalized_sha256 "$cap8_sha" \
    '{only_route_max_load_units_varies: true, normalized_sha256: $normalized_sha256}' \
    >"$proof"
}

run_cell() {
  local label=$1 cap=$2 prefill=$3 decoders=$4 decode=$5 runs=$6 lead_ms=$7 max_seconds=$8
  local started result thermal guard_stdout
  result=$experiment_dir/$label.json
  thermal=$experiment_dir/$label-thermal.jsonl
  guard_stdout=$experiment_dir/$label-guard.stdout
  [[ ! -e $result && ! -e $thermal && ! -e $guard_stdout ]] || \
    fail "$label evidence already exists"
  ((SECONDS - campaign_started < campaign_max_seconds)) || \
    fail "campaign exceeded its wall-time authority"
  started=$(date -u +%FT%TZ)
  python3 "$experiment_dir/node06_gpu_guard.py" \
    --output "$thermal" \
    --label "qwen38-$label" \
    --max-runtime-seconds "$max_seconds" \
    -- \
    env \
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
  jq -e 'all(.run_route_relationships[]; .decoder_unknown_route == 0)' \
    "$result" >/dev/null || fail "$label route authority was incomplete"
  for engine in "${engines[@]}"; do
    if docker logs --since "$started" "$engine" 2>&1 |
      grep -Eiq 'CUDA.*error|NCCL.*error|out of memory|Xid|Traceback|EngineCore.*fail|JIT|compil'; then
      fail "$label produced a new engine failure marker"
    fi
  done
  require_lb "$cap"
  require_engines_unchanged "$experiment_dir/$label-engines-after.txt"
  ((SECONDS - campaign_started < campaign_max_seconds)) || \
    fail "campaign exceeded its wall-time authority"
}

campaign_started=$SECONDS
engine_state >"$experiment_dir/engines.before.txt"
require_lb 8
prove_render_delta
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
wait_for_idle

# Prove the exact merged harness before the first load-balancer mutation.
run_cell smoke 8 32000 2 64 1 200 "$smoke_max_seconds"

render_and_recreate 8 cap8-a1
run_cell cap8-a1 8 128000 16 512 3 200 "$full_max_seconds"
render_and_recreate 32 cap32-b1
run_cell cap32-b1 32 128000 16 512 3 200 "$full_max_seconds"
render_and_recreate 32 cap32-b2
run_cell cap32-b2 32 128000 16 512 3 200 "$full_max_seconds"
render_and_recreate 8 cap8-a2
run_cell cap8-a2 8 128000 16 512 3 200 "$full_max_seconds"

require_lb 8
require_engines_unchanged "$experiment_dir/engines.after.txt"
jq -n \
  --slurpfile a1 "$experiment_dir/cap8-a1.json" \
  --slurpfile a2 "$experiment_dir/cap8-a2.json" \
  --slurpfile b1 "$experiment_dir/cap32-b1.json" \
  --slurpfile b2 "$experiment_dir/cap32-b2.json" '
  def mean($left; $right): ($left + $right) / 2;
  mean($a1[0].decoder_ttft_ms_p95; $a2[0].decoder_ttft_ms_p95) as $a_ttft |
  mean($b1[0].decoder_ttft_ms_p95; $b2[0].decoder_ttft_ms_p95) as $b_ttft |
  mean($a1[0].decoder_aggregate_tok_s_median; $a2[0].decoder_aggregate_tok_s_median) as $a_tps |
  mean($b1[0].decoder_aggregate_tok_s_median; $b2[0].decoder_aggregate_tok_s_median) as $b_tps |
  mean($a1[0].prefill_ttft_ms_p95; $a2[0].prefill_ttft_ms_p95) as $a_prefill |
  mean($b1[0].prefill_ttft_ms_p95; $b2[0].prefill_ttft_ms_p95) as $b_prefill |
  (all(($b1[0].run_route_relationships + $b2[0].run_route_relationships)[];
    .decoder_other_route >= 14)) as $placement |
  (($b_ttft <= 0.80 * $a_ttft and $b_tps >= 0.95 * $a_tps) or
   ($b_tps >= 1.10 * $a_tps and $b_ttft <= $a_ttft / 0.95)) as $service |
  ($b_prefill <= 1.10 * $a_prefill) as $prefill_guard |
  {
    baseline_cap: 8,
    candidate_cap: 32,
    decoder_ttft_ms_p95: {baseline: $a_ttft, candidate: $b_ttft},
    decoder_aggregate_tok_s_median: {baseline: $a_tps, candidate: $b_tps},
    prefill_ttft_ms_p95: {baseline: $a_prefill, candidate: $b_prefill},
    candidate_placement_gate: $placement,
    candidate_service_gate: $service,
    candidate_prefill_guard: $prefill_guard,
    primary_gate_passed: ($placement and $service and $prefill_guard),
    promotion_applied: false,
    remaining_promotion_guards: ["decode-first", "c32-code", "serial-cache"]
  }
' >"$experiment_dir/comparison.json"
mutated=0
trap - EXIT INT TERM HUP
printf '%s\n' 'qwen38 route-load cap evidence captured; cap 8 remains live'
