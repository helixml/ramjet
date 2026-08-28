#!/usr/bin/env bash
# Guarded one-engine qualification of the pinned upstream NVFP4 recipe.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
compose_file=$deployment_dir/docker-compose.yaml
compose_sha=826e3b4f11b06a80c2deca40f0e1d089a040fe3ae4dc7b001e54e01b89cc72d6
candidate_compose_sha=48b9e161e3aff275b7a0b31ce3cf351db97401714887b48c365eaf8327e0092b
model_root=/prod/models/Inferact/Qwen3.8-Flash-Next-NVFP4-103a76083161
model_revision=103a7608316173ca6edd49929544244de7ffda70
model_manifest_sha=050debd7dc6e22a4ceb5aafb8b8cb7629f2f7ce60859b1605928cf8f7eb0ff58
lock_file=/run/lock/ramjet-node06-deployment.lock
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
engine_image='vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000'
single_upstream='http://qwen38flashnext-a:8000'
engine=qwen38flashnext-b
peer=qwen38flashnext-a
model=qwen3.8-flash-next
expected_fp8_kv_bytes=40190174004
expected_fp8_kv_tokens=2667258
mtp_argument='--speculative-config={"method":"mtp","num_speculative_tokens":3,"index_share_for_mtp_iteration":true}'

fail() {
  echo "qwen NVFP4 qualification: $*" >&2
  exit 2
}

[[ $# == 1 || $# == 2 ]] || fail "usage: $0 [--prepare] EXISTING-EXPERIMENT-DIRECTORY"
mode=run
if [[ ${1:-} == --prepare ]]; then
  mode=prepare
  shift
fi
[[ $(hostname) == node06 ]] || fail "this campaign may run only on node06"
experiment_dir=$(realpath -e -- "$1")
runner=$(realpath -e -- "$0")
candidate_compose=$experiment_dir/docker-compose.nvfp4.yaml
model_verification=$experiment_dir/model-verification.json
[[ $experiment_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "experiment directory is outside the deployment"
[[ $(stat -c '%u:%a' "$experiment_dir") == 0:700 ]] ||
  fail "experiment directory must be root-owned mode 0700"
[[ $runner == "$experiment_dir/qwen38_nvfp4_qualify.sh" ]] ||
  fail "execute the staged campaign authority"
[[ $(sha256sum "$compose_file" | awk '{print $1}') == "$compose_sha" ]] ||
  fail "canonical Compose bytes drifted"

artifacts=(
  agentbench.py agent_cases_v1.jsonl agent_cases_v2_sessions.jsonl
  agent_cases_v2_deep_context.jsonl codebench.py engine_greedy_ab.py
  engine_metrics.py multimodal_smoke.py qwen38_nvfp4_args_preflight.py
  qwen38_nvfp4_compose.py qwen38_nvfp4_model_verify.py
  node06_agent_metadata.sh capture_node06.sh node06_gpu_guard.py
  node06_operational_moratorium.py
)
for artifact in "${artifacts[@]}"; do
  [[ -f $experiment_dir/$artifact && ! -L $experiment_dir/$artifact ]] ||
    fail "missing staged artifact: $artifact"
done

compose() {
  local file=$1 upstreams=$2
  shift 2
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    docker compose -f "$file" --project-directory "$deployment_dir" "$@"
}

if [[ $mode == prepare ]]; then
  [[ ${RAMJET_GPU_GUARD_ACTIVE:-0} != 1 ]] ||
    fail "model preparation must run outside the inference guard"
  [[ ! -e $candidate_compose && ! -e $model_verification ]] ||
    fail "candidate preparation outputs already exist"
  umask 077
  python3 "$experiment_dir/qwen38_nvfp4_model_verify.py" "$model_root" \
    >"$model_verification"
  python3 "$experiment_dir/qwen38_nvfp4_compose.py" \
    "$compose_file" "$candidate_compose" \
    >"$experiment_dir/candidate-compose-sha256.txt"
  [[ $(sha256sum "$candidate_compose" | awk '{print $1}') == "$candidate_compose_sha" ]] ||
    fail "NVFP4 candidate Compose derivation changed"
  compose "$candidate_compose" "$single_upstream" config --format json \
    >"$experiment_dir/candidate-compose.json"
  jq '.services["qwen38flashnext-b"].command' \
    "$experiment_dir/candidate-compose.json" >"$experiment_dir/candidate-argv.json"
  docker run --rm --network none --entrypoint python3 \
    -v "$experiment_dir:/probe:ro" "$engine_image" \
    /probe/qwen38_nvfp4_args_preflight.py \
    >"$experiment_dir/engine-args-preflight.txt"
  sha256sum "$model_verification" | awk '{print $1}'
  exit 0
fi

[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
for prepared in "$candidate_compose" "$model_verification" \
  "$experiment_dir/candidate-compose.json" "$experiment_dir/candidate-argv.json" \
  "$experiment_dir/engine-args-preflight.txt"; do
  [[ -f $prepared && ! -L $prepared ]] || fail "missing prepared authority: $prepared"
done
[[ $(sha256sum "$candidate_compose" | awk '{print $1}') == "$candidate_compose_sha" ]] ||
  fail "prepared candidate Compose changed"
[[ $model_manifest_sha != TO_BE_PREPARED ]] ||
  fail "runner has not pinned the prepared model manifest"
[[ $(sha256sum "$model_verification" | awk '{print $1}') == "$model_manifest_sha" ]] ||
  fail "prepared model verification changed"
jq -e --arg revision "$model_revision" '
  .schema_version == 1 and .repository == "Inferact/Qwen3.8-Flash-Next-NVFP4" and
  .revision == $revision and .files == 34 and .total_bytes == 182838060595 and
  .safetensor_bytes == 182779284200 and .verified == true
' "$model_verification" >/dev/null || fail "prepared model authority is invalid"
[[ ! -L $model_root && $(stat -c '%u:%a' "$model_root") == 0:755 ]] ||
  fail "model root authority changed after verification"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"
export BENCH_TOKEN=$VLLM_API_KEY

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

set -o noclobber
sha256sum "$runner" "$candidate_compose" "$model_verification" \
  "$experiment_dir/candidate-compose.json" "$experiment_dir/candidate-argv.json" \
  "$experiment_dir/engine-args-preflight.txt" \
  "${artifacts[@]/#/$experiment_dir/}" \
  >"$experiment_dir/campaign-authority.sha256"
set +o noclobber

engine_cmd() {
  docker inspect --format '{{json .Config.Cmd}}' "$engine"
}

check_baseline_shape() {
  local inspect cmd
  inspect=$(docker inspect "$engine") || return 1
  jq -e --arg image "$engine_image" '
    length == 1 and .[0].Config.Image == $image and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "Qwen/Qwen3.8-Flash-Next-FP8" and
    .[0].Config.Labels["ai.ramjet.model.revision"] == "bcd9f01ddc9cff2316eb84281bebcd5b058bddce" and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
  ' <<<"$inspect" >/dev/null || return 1
  cmd=$(engine_cmd) || return 1
  jq -e --arg kv "--kv-cache-memory=$expected_fp8_kv_bytes" --arg mtp "$mtp_argument" '
    index($kv) != null and index($mtp) != null and index("--max-num-seqs=64") != null and
    ([.[] | select(startswith("--kv-cache-memory="))] | length) == 1 and
    ([.[] | select(contains("speculative") or startswith("--spec-"))] | length) == 1
  ' <<<"$cmd" >/dev/null
}

check_candidate_shape() {
  local inspect cmd
  inspect=$(docker inspect "$engine") || return 1
  jq -e --arg image "$engine_image" --arg revision "$model_revision" --arg source "$model_root" '
    length == 1 and .[0].Config.Image == $image and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "Inferact/Qwen3.8-Flash-Next-NVFP4" and
    .[0].Config.Labels["ai.ramjet.model.revision"] == $revision and
    ([.[0].Mounts[] | select(.Source == $source and .Destination == "/workspace/model" and .RW == false)] | length) == 1 and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
  ' <<<"$inspect" >/dev/null || return 1
  cmd=$(engine_cmd) || return 1
  jq -e --arg revision "--revision=$model_revision" '
    index($revision) != null and index("--tokenizer-revision=103a7608316173ca6edd49929544244de7ffda70") != null and
    index("--max-num-seqs=16") != null and index("--max-num-batched-tokens=8192") != null and
    index("--gpu-memory-utilization=0.95") != null and index("--moe-backend=marlin") != null and
    ([.[] | select(startswith("--kv-cache-memory="))] | length) == 0 and
    ([.[] | select(contains("speculative") or startswith("--spec-"))] | length) == 0
  ' <<<"$cmd" >/dev/null
}

check_baseline_kv() {
  curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/metrics |
    grep -E "^vllm:cache_config_info[{].*kv_cache_memory_bytes=\"$expected_fp8_kv_bytes\".*kv_cache_size_tokens=\"$expected_fp8_kv_tokens\"" \
      >/dev/null
}

check_candidate_kv() {
  local line tokens
  line=$(curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/metrics | grep -E '^vllm:cache_config_info[{]' | head -n1) || return 1
  tokens=$(sed -n 's/.*kv_cache_size_tokens="\([0-9][0-9]*\)".*/\1/p' <<<"$line")
  [[ $tokens =~ ^[0-9]+$ && $tokens -ge 1000000 ]] || return 1
}

wait_engine() {
  local shape=$1 kv=$2 deadline=$((SECONDS + 900))
  until "$shape" && "$kv" &&
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
      .status == "ok" and .healthy_replicas == $expected and .total_replicas == $expected
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
  if ! check_baseline_shape; then
    compose "$compose_file" "$single_upstream" up -d --no-deps --force-recreate "$engine" \
      >"$experiment_dir/rollback-engine.txt" 2>&1
  fi
  wait_engine check_baseline_shape check_baseline_kv || rollback_rc=1
  recreate_lb "$all_upstreams" 2 || rollback_rc=1
  record_engine final || rollback_rc=1
  if ((rollback_rc != 0)); then
    echo "qwen NVFP4 qualification: rollback verification failed" >&2
    exit 3
  fi
  exit "$original_rc"
}
trap rollback EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

check_baseline_shape || fail "engine B is not the exact FP8/MTP3 baseline"
check_baseline_kv || fail "engine B does not have the exact FP8/MTP3 KV pool"
peer_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
bash "$experiment_dir/capture_node06.sh" --local --profile qwen38-flash-next \
  >"$experiment_dir/preflight.txt"
record_engine initial
recreate_lb "$single_upstream" 1

candidate_started=$(date +%s)
compose "$candidate_compose" "$single_upstream" up -d --no-deps --force-recreate "$engine" \
  >"$experiment_dir/candidate-recreate.txt" 2>&1
wait_engine check_candidate_shape check_candidate_kv || fail "NVFP4 candidate did not become ready"
printf '%s\n' "$(( $(date +%s) - candidate_started ))" \
  >"$experiment_dir/candidate-readiness-seconds.txt"
record_engine candidate
docker logs "$engine" 2>&1 |
  grep -Ei 'nvfp4|modelopt|marlin|cache size|available kv|loading model weights' |
  tail -n 200 >"$experiment_dir/candidate-runtime-markers.txt" || true

BENCH_GPU_COUNT=4 BENCH_MODEL_ROOT="$model_root" BENCH_MODEL_REVISION="$model_revision" \
  bash "$experiment_dir/node06_agent_metadata.sh" \
  "$experiment_dir/candidate-agent-metadata.json" "$engine"

run_agent() {
  local label=$1 corpus=$2
  python3 "$experiment_dir/agentbench.py" run \
    http://127.0.0.1:8041 "$model" --corpus "$experiment_dir/$corpus" \
    --metadata-json "$experiment_dir/candidate-agent-metadata.json" \
    --profile deterministic --concurrency 1 --repetitions 1 \
    --salt "${label}-$(date +%s%N)" --timeout 900 \
    --engine-metrics http://127.0.0.1:8041/metrics \
    --speculation-mode disabled --require-reconciled-speculation \
    >"$experiment_dir/${label}.jsonl"
}
run_agent candidate-agent-v1 agent_cases_v1.jsonl
run_agent candidate-agent-sessions agent_cases_v2_sessions.jsonl
run_agent candidate-agent-deep-context agent_cases_v2_deep_context.jsonl

BENCH_SPEC_MODE=disabled BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
  python3 "$experiment_dir/multimodal_smoke.py" \
  http://127.0.0.1:8041 "$model" --engine-metrics http://127.0.0.1:8041/metrics \
  --require-reconciled-speculation --spec-mode disabled \
  >"$experiment_dir/candidate-multimodal.json"

python3 "$experiment_dir/engine_greedy_ab.py" \
  http://127.0.0.1:8041 http://127.0.0.1:8040 \
  --a-name candidate --b-name baseline --model "$model" \
  >"$experiment_dir/candidate-greedy-ab.jsonl"
jq -e '
  .summary == true and .n == 8 and
  .candidate_correct >= 7 and .baseline_correct >= 7 and
  .candidate_correct >= .baseline_correct
' \
  < <(tail -n 1 "$experiment_dir/candidate-greedy-ab.jsonl") >/dev/null ||
  fail "greedy candidate regressed against the measured baseline"

base_prompt='Write a complete, production-quality Python module that implements a thread-safe LRU cache with TTL expiry. Include the full class with type hints, mapping methods, a background sweeper thread, explicit locking, stats, and tests. Output only code.'
for concurrency in 1 8 16 32; do
  BENCH_PROMPT="$base_prompt Experiment namespace: nvfp4-c${concurrency}." \
    BENCH_SPEC_MODE=disabled BENCH_REQUIRE_RECONCILED_SPECULATION=1 \
    METRICS_URL=http://127.0.0.1:8041/metrics SWEEP_LABEL="nvfp4-c${concurrency}" \
    python3 "$experiment_dir/codebench.py" \
      http://127.0.0.1:8041 "$model" 256 "$concurrency" 2 \
      >"$experiment_dir/candidate-c${concurrency}.json"
done

[[ $peer_before == "$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] ||
  fail "healthy peer changed during the campaign"
printf '%s\n' 'qualification complete; restoring exact production baseline'
