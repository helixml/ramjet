#!/usr/bin/env bash
# Finish an exact node06 Qwen3.8 baseline/LB restoration after a bounded run.
set -Eeuo pipefail

deployment_dir=/home/luke/inference/qwen38_flash_next
canonical_compose=$deployment_dir/docker-compose.yaml
canonical_sha=9dc3e797bee511d5f3b6bb6022c47471db7c054885c1141f4f982bd270c9a847
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
baseline_image='sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
lb_image='ghcr.io/helixml/ramjet:rust-ff8a4af@sha256:e4d71dbbe7050b336dbc1ff6ad28c3f2235ee963f29f4524cf8ed075dbbeb5b0'
all_upstreams='http://qwen38flashnext-a:8000,http://qwen38flashnext-b:8000,http://qwen38flashnext-tp8:8000'
single_upstream='http://qwen38flashnext-a:8000'
all_speculation_profiles='mtp,standard,mtp'
single_speculation_profile='mtp'
all_kv_live='tcp://qwen38flashnext-a:5557,tcp://qwen38flashnext-b:5557,tcp://qwen38flashnext-tp8:5557'
all_kv_replay='tcp://qwen38flashnext-a:5558,tcp://qwen38flashnext-b:5558,tcp://qwen38flashnext-tp8:5558'
single_kv_live='tcp://qwen38flashnext-a:5557'
single_kv_replay='tcp://qwen38flashnext-a:5558'

fail() {
  echo "qwen baseline restoration: $*" >&2
  exit 2
}

[[ $# == 1 ]] || fail "usage: $0 EXISTING-EVIDENCE-DIRECTORY"
[[ $(hostname) == node06 ]] || fail "this restoration may run only on node06"
[[ ${RAMJET_GPU_GUARD_ACTIVE:-} == 1 ]] || fail "GPU guard is not active"
evidence_dir=$(realpath -e -- "$1")
[[ $evidence_dir == "$deployment_dir/.experiments/"* ]] ||
  fail "evidence directory is outside the deployment"
[[ $(stat -c '%u:%a' "$evidence_dir") == 0:700 ]] ||
  fail "evidence directory must be root-owned mode 0700"
[[ $(sha256sum "$canonical_compose" | awk '{print $1}') == "$canonical_sha" ]] ||
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
  local upstreams=$1 speculation_profiles=$2 speculation_mode=$3
  local kv_live=$4 kv_replay=$5
  shift 5
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$upstreams" \
    RJ_ROUTE_SPECULATION_PROFILES="$speculation_profiles" \
    RJ_ROUTE_SPECULATION_MODE="$speculation_mode" \
    RJ_KV_EVENT_LIVE_ENDPOINTS="$kv_live" \
    RJ_KV_EVENT_REPLAY_ENDPOINTS="$kv_replay" \
    docker compose -f "$canonical_compose" --project-directory "$deployment_dir" "$@"
}

wait_engine() {
  local deadline=$((SECONDS + 900)) inspect
  until inspect=$(docker inspect "$engine" 2>/dev/null) &&
  jq -e --arg image "$baseline_image" '
    length == 1 and
    (.[0].Image == $image or .[0].Config.Image == $image) and
    .[0].Config.Labels["ai.ramjet.model.repository"] == "Qwen/Qwen3.8-Flash-Next-FP8" and
    .[0].Config.Labels["ai.ramjet.model.revision"] == "bcd9f01ddc9cff2316eb84281bebcd5b058bddce" and
    .[0].State.Status == "running" and .[0].State.OOMKilled == false and
    .[0].RestartCount == 0
  ' <<<"$inspect" >/dev/null &&
  curl -fsS --max-time 5 -H "Authorization: Bearer $VLLM_API_KEY" \
    http://127.0.0.1:8041/health >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 5
  done
}

wait_lb() {
  local expected_healthy=$1 expected_total=$2
  local deadline=$((SECONDS + 90)) health
  until health=$(curl -fsS --max-time 5 http://127.0.0.1:8006/health 2>/dev/null) &&
    jq -e --argjson healthy "$expected_healthy" --argjson total "$expected_total" '
      .status == "ok" and .healthy_replicas == $healthy and
      .active_replicas == $healthy and .total_replicas == $total
    ' <<<"$health" >/dev/null; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

peer_before=$(docker inspect --format \
  '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")
compose "$single_upstream" "$single_speculation_profile" off \
  "$single_kv_live" "$single_kv_replay" \
  up -d --no-deps --force-recreate ds4-loadbalancer \
  >"$evidence_dir/recovery-lb-single.txt" 2>&1
wait_lb 1 1 || fail "single-home load balancer did not become ready"
compose "$single_upstream" "$single_speculation_profile" off \
  "$single_kv_live" "$single_kv_replay" \
  up -d --no-deps --force-recreate "$engine" \
  >"$evidence_dir/recovery-engine.txt" 2>&1
wait_engine || fail "exact baseline engine did not become ready"
compose "$all_upstreams" "$all_speculation_profiles" prefer \
  "$all_kv_live" "$all_kv_replay" \
  up -d --no-deps --force-recreate ds4-loadbalancer \
  >"$evidence_dir/recovery-lb-all.txt" 2>&1
wait_lb 2 3 || fail "restored load balancer did not become ready"
[[ $peer_before == "$(docker inspect --format \
  '{{.Id}} {{.Image}} {{.State.StartedAt}} {{.RestartCount}}' "$peer")" ]] ||
  fail "healthy peer changed during restoration"
{
  docker inspect ds4-loadbalancer "$peer" "$engine" --format \
    '{{.Name}} {{.Image}} {{.State.Status}} {{.RestartCount}} {{.State.OOMKilled}}'
  curl -fsS --max-time 5 http://127.0.0.1:8006/health
} >"$evidence_dir/recovery-final.txt"
