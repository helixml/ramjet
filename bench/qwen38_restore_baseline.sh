#!/usr/bin/env bash
# Recover the exact Qwen FP8/MTP3 B engine and two-upstream load balancer.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
compose_file=$deployment_dir/docker-compose.yaml
compose_sha=826e3b4f11b06a80c2deca40f0e1d089a040fe3ae4dc7b001e54e01b89cc72d6
lock_file=/run/lock/ramjet-node06-deployment.lock
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
engine_image='vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000'
single_upstream='http://qwen38flashnext-a:8000'
engine=qwen38flashnext-b
peer=qwen38flashnext-a
expected_kv_bytes=40190174004
expected_kv_tokens=2667258
mtp_argument='--speculative-config={"method":"mtp","num_speculative_tokens":3,"index_share_for_mtp_iteration":true}'

fail() {
  echo "qwen baseline recovery: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EVIDENCE-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this recovery may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
evidence_dir=$(realpath -e -- "$1")
[[ $evidence_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "evidence directory is outside the deployment"
[[ $(stat -c '%u:%a' "$evidence_dir") == 0:700 ]] ||
  fail "evidence directory must be root-owned mode 0700"
[[ $(sha256sum "$compose_file" | awk '{print $1}') == "$compose_sha" ]] ||
  fail "canonical Compose bytes drifted"

set -a
# shellcheck disable=SC1091
source "$deployment_dir/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

compose() {
  local upstreams=$1
  shift
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    docker compose -f "$compose_file" --project-directory "$deployment_dir" "$@"
}

check_engine() {
  local inspect cmd
  inspect=$(docker inspect "$engine") || return 1
  jq -e --arg image "$engine_image" '
    length == 1 and .[0].Config.Image == $image and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "Qwen/Qwen3.8-Flash-Next-FP8" and
    .[0].Config.Labels["ai.ramjet.model.revision"] == "bcd9f01ddc9cff2316eb84281bebcd5b058bddce" and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
  ' <<<"$inspect" >/dev/null || return 1
  cmd=$(docker inspect --format '{{json .Config.Cmd}}' "$engine") || return 1
  jq -e --arg kv "--kv-cache-memory=$expected_kv_bytes" --arg mtp "$mtp_argument" '
    index($kv) != null and index($mtp) != null and index("--max-num-seqs=64") != null
  ' <<<"$cmd" >/dev/null || return 1
  curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/metrics |
    grep -E "^vllm:cache_config_info[{].*kv_cache_memory_bytes=\"$expected_kv_bytes\".*kv_cache_size_tokens=\"$expected_kv_tokens\"" \
      >/dev/null || return 1
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/health >/dev/null
}

wait_engine() {
  local deadline=$((SECONDS + 900))
  until check_engine; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_lb() {
  local deadline=$((SECONDS + 90)) health
  until health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null) &&
    jq -e '.status == "ok" and .healthy_replicas == 2 and .total_replicas == 2' \
      <<<"$health" >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

peer_before=$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
compose "$single_upstream" up -d --no-deps --force-recreate "$engine" \
  >"$evidence_dir/recovery-engine.txt" 2>&1
wait_engine || fail "exact FP8/MTP3 engine B did not recover"
compose "$all_upstreams" up -d --no-deps --force-recreate ds4-loadbalancer \
  >"$evidence_dir/recovery-lb.txt" 2>&1
wait_lb || fail "two-upstream load balancer did not recover"
[[ $peer_before == "$(docker inspect --format '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] ||
  fail "healthy peer changed during recovery"
docker inspect --format \
  '{{.Name}} {{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{.State.OOMKilled}}' \
  "$peer" "$engine" >"$evidence_dir/recovery-final.txt"
