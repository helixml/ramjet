#!/usr/bin/env bash
# Guarded same-engine A/B/B/A qualification of Qwen Flash-Next MTP3 versus off.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
compose_file=$deployment_dir/docker-compose.yaml
compose_sha=826e3b4f11b06a80c2deca40f0e1d089a040fe3ae4dc7b001e54e01b89cc72d6
candidate_compose_sha=7457bafce5b6e376d0c79cb08caf90c61cf6a3729ab61d70633cfa1870e3f53d
lock_file=/run/lock/ramjet-node06-deployment.lock
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
engine_image='vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000'
single_upstream='http://qwen38flashnext-a:8000'
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
expected_kv_bytes=40190174004
expected_mtp_kv_tokens=2667258
expected_plain_kv_tokens=3033380
mtp_argument='--speculative-config={"method":"mtp","num_speculative_tokens":3,"index_share_for_mtp_iteration":true}'

fail() {
  echo "qwen MTP A/B: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EXPERIMENT-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
candidate_compose=$experiment_dir/docker-compose.mtp-off.yaml
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] ||
  fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_mtp_abba.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$compose_file" | awk '{print $1}') == "$compose_sha" ]] ||
  fail "canonical Compose bytes drifted"

for artifact in agentbench.py agent_cases_v1.jsonl codebench.py engine_metrics.py \
  qwen38_mtp_args_preflight.py node06_agent_metadata.sh capture_node06.sh \
  node06_gpu_guard.py node06_operational_moratorium.py; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] ||
    fail "missing staged artifact: $artifact"
done

# The candidate is a complete standalone Compose file, never an overlay. Its
# only byte delta is deletion of the one speculative-config command element.
[[ $(grep -c -- '^[[:space:]]*- --speculative-config=' "$compose_file") == 1 ]] ||
  fail "canonical Compose does not contain exactly one MTP argument"
umask 077
set -o noclobber
awk '!/^[[:space:]]*- --speculative-config=/' "$compose_file" >"$candidate_compose"
set +o noclobber
[[ $(sha256sum "$candidate_compose" | awk '{print $1}') == "$candidate_compose_sha" ]] ||
  fail "MTP-off Compose derivation changed"
[[ $(grep -c -- 'speculative-config' "$candidate_compose") == 0 ]] ||
  fail "MTP-off Compose still contains speculative decoding"

# Direct engine auth is deployment-local and never enters argv or evidence.
set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export BENCH_TOKEN=$VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

compose() {
  local file=$1 upstreams=$2
  shift 2
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    docker compose -f "$file" --project-directory "$deployment_dir" "$@"
}

# Render and parser-admit the exact candidate without a network or GPU before
# withdrawing or recreating an engine.
compose "$candidate_compose" "$single_upstream" config --format json \
  >"$experiment_dir/candidate-compose.json"
jq -e '.services["qwen38flashnext-b"].command |
  ([.[] | select(contains("speculative") or startswith("--spec-"))] | length) == 0' \
  "$experiment_dir/candidate-compose.json" >/dev/null ||
  fail "rendered candidate still enables speculation"
jq '.services["qwen38flashnext-b"].command' \
  "$experiment_dir/candidate-compose.json" >"$experiment_dir/candidate-argv.json"
docker run --rm --network none --entrypoint python3 \
  -v "$experiment_dir:/probe:ro" "$engine_image" \
  /probe/qwen38_mtp_args_preflight.py \
  >"$experiment_dir/engine-args-preflight.txt"

set -o noclobber
sha256sum \
  "$runner" \
  "$candidate_compose" \
  "$experiment_dir/candidate-argv.json" \
  "$experiment_dir/agentbench.py" \
  "$experiment_dir/agent_cases_v1.jsonl" \
  "$experiment_dir/codebench.py" \
  "$experiment_dir/engine_metrics.py" \
  "$experiment_dir/qwen38_mtp_args_preflight.py" \
  "$experiment_dir/node06_agent_metadata.sh" \
  "$experiment_dir/capture_node06.sh" \
  "$experiment_dir/node06_gpu_guard.py" \
  "$experiment_dir/node06_operational_moratorium.py" \
  >"$experiment_dir/campaign-authority.sha256"
set +o noclobber

engine_cmd() {
  docker inspect --format '{{json .Config.Cmd}}' "$engine"
}

check_engine_shape() {
  local mode=$1 cmd inspect
  inspect=$(docker inspect "$engine") || return 1
  jq -e --arg image "$engine_image" '
    length == 1 and .[0].Config.Image == $image and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
  ' <<<"$inspect" >/dev/null || return 1
  cmd=$(engine_cmd) || return 1
  jq -e --arg kv "--kv-cache-memory=$expected_kv_bytes" '
    index($kv) != null and index("--max-num-seqs=64") != null and
    ([.[] | select(startswith("--kv-cache-memory="))] | length) == 1 and
    ([.[] | select(startswith("--max-num-seqs="))] | length) == 1
  ' <<<"$cmd" >/dev/null || return 1
  if [[ $mode == enabled ]]; then
    jq -e --arg mtp "$mtp_argument" '
      index($mtp) != null and
      ([.[] | select(contains("speculative") or startswith("--spec-"))] | length) == 1
    ' <<<"$cmd" >/dev/null
  else
    jq -e '
      ([.[] | select(contains("speculative") or startswith("--spec-"))] | length) == 0
    ' <<<"$cmd" >/dev/null
  fi
}

check_kv_pool() {
  local mode=$1 expected_tokens=$expected_mtp_kv_tokens
  [[ $mode == enabled ]] || expected_tokens=$expected_plain_kv_tokens
  curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/metrics |
    grep -E "^vllm:cache_config_info[{].*kv_cache_memory_bytes=\"$expected_kv_bytes\".*kv_cache_size_tokens=\"$expected_tokens\"" \
      >/dev/null
}

wait_engine() {
  local mode=$1 deadline=$((SECONDS + 900))
  until check_engine_shape "$mode" && check_kv_pool "$mode" &&
    curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
      http://127.0.0.1:8041/health >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_lb() {
  local expected=$1 deadline=$((SECONDS + 90)) health
  until health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null) &&
    jq -e --argjson expected "$expected" '
      .status == "ok" and .healthy_replicas == $expected and
      .total_replicas == $expected
    ' <<<"$health" >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

recreate_lb() {
  local upstreams=$1 expected=$2
  compose "$compose_file" "$upstreams" up -d --no-deps --force-recreate ds4-loadbalancer \
    >"$experiment_dir/lb-${expected}-recreate.txt" 2>&1
  wait_lb "$expected"
}

record_engine() {
  local label=$1
  {
    date -u +%FT%TZ
    docker inspect --format \
      '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' \
      "$peer" "$engine"
    engine_cmd
    curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
      http://127.0.0.1:8041/metrics |
      grep -E 'vllm:gpu_cache_usage_perc|vllm:num_requests_(running|waiting)|vllm:cache_config_info'
  } >"$experiment_dir/$label.txt"
}

rollback() {
  local original_rc=$? rollback_rc=0
  trap - EXIT INT TERM
  set +e
  if ! check_engine_shape enabled; then
    compose "$compose_file" "$single_upstream" up -d --no-deps --force-recreate "$engine" \
      >"$experiment_dir/rollback-engine.txt" 2>&1
  fi
  wait_engine enabled || rollback_rc=1
  recreate_lb "$all_upstreams" 2 || rollback_rc=1
  record_engine final || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "qwen MTP A/B: rollback verification failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

check_engine_shape enabled || fail "engine B is not the exact MTP3 baseline"
peer_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
record_engine initial

# Remove B from production before any direct baseline traffic or restart.
recreate_lb "$single_upstream" 1

base_prompt='Write a complete, production-quality Python module that implements a thread-safe LRU cache with TTL expiry. Include the full class with type hints, mapping methods, a background sweeper thread, explicit locking, stats, and tests. Output only code.'

run_code_round() {
  local label=$1 mode=$2 concurrency
  for concurrency in 1 8 16 32; do
    BENCH_PROMPT="$base_prompt Experiment namespace: ${label}-c${concurrency}." \
      BENCH_SPEC_MODE="$mode" \
      BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
      METRICS_URL=http://127.0.0.1:8041/metrics \
      SWEEP_LABEL="${label}-c${concurrency}" \
      python3 "$experiment_dir/codebench.py" \
        http://127.0.0.1:8041 "$model" 256 "$concurrency" 2 \
        >"$experiment_dir/${label}-c${concurrency}.json"
  done
}

run_agent() {
  local label=$1 mode=$2
  BENCH_GPU_COUNT=4 \
  BENCH_MODEL_ROOT=/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c \
  BENCH_MODEL_REVISION=bcd9f01ddc9cff2316eb84281bebcd5b058bddce \
    bash "$experiment_dir/node06_agent_metadata.sh" \
    "$experiment_dir/${label}-agent-metadata.json" "$engine"
  python3 "$experiment_dir/agentbench.py" run \
    http://127.0.0.1:8041 "$model" \
    --corpus "$experiment_dir/agent_cases_v1.jsonl" \
    --metadata-json "$experiment_dir/${label}-agent-metadata.json" \
    --profile deterministic --concurrency 1 --repetitions 1 \
    --salt "${label}-agent-$(date +%s%N)" \
    --engine-metrics http://127.0.0.1:8041/metrics \
    --speculation-mode "$mode" --require-reconciled-speculation \
    >"$experiment_dir/${label}-agent.jsonl"
}

# Prove every correctness artifact and invocation before paying for a reload.
run_agent mtp-on-preflight enabled
run_code_round mtp-on-a1 enabled

candidate_started=$(date +%s)
compose "$candidate_compose" "$single_upstream" up -d --no-deps --force-recreate "$engine" \
  >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine disabled || fail "MTP-off candidate did not become ready"
printf '%s\n' "$(( $(date +%s) - candidate_started ))" \
  >"$experiment_dir/candidate-readiness-seconds.txt"
record_engine candidate
run_agent mtp-off-candidate disabled
run_code_round mtp-off-b1 disabled
run_code_round mtp-off-b2 disabled

baseline_started=$(date +%s)
compose "$compose_file" "$single_upstream" up -d --no-deps --force-recreate "$engine" \
  >"$experiment_dir/baseline-recreate.txt" 2>&1
wait_engine enabled || fail "MTP3 baseline did not recover"
printf '%s\n' "$(( $(date +%s) - baseline_started ))" \
  >"$experiment_dir/baseline-readiness-seconds.txt"
run_code_round mtp-on-a2 enabled

[[ $peer_before == "$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] ||
  fail "healthy peer changed during the campaign"

# EXIT owns the exact MTP3 and LB=2/2 restoration and final evidence.
printf '%s\n' 'measurement complete; restoring exact production baseline'
