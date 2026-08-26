#!/usr/bin/env bash
# Isolate GPUs 4-7 for the Qwen3.8-Flash-Next canary, or restore the baseline.
set -Eeuo pipefail

action=${1:-}
old_dir=/home/luke/inference/qwen38_27b
new_dir=/home/luke/inference/qwen38_flash_next
lock=/run/lock/ramjet-node06-deployment.lock
model_dir=/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c
candidate_image='vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3'
lb_image='ghcr.io/helixml/ramjet:rust-r133-qwen38-flash-next-df01c18@sha256:78f13c87fcc928552593a8055293479dbbc2569d0b7a4b754d89e0d32a278385'
old_compose_sha256=fe4275830c555ab59fd77c23e43a3ce53baa06a94577fdef68cc4c2ec117f242
new_compose_sha256=f8ef22bd53edfbe264b78f0d3f24e4fec1432103c97ac6d7ba899e28048a1553
single_upstreams='http://qwen38-sg-e0:8000,http://qwen38-sg-e1:8000,http://qwen38-sg-e2:8000,http://qwen38-sg-e3:8000'
full_upstreams="${single_upstreams},http://qwen38-sg-e4:8000,http://qwen38-sg-e5:8000,http://qwen38-sg-e6:8000,http://qwen38-sg-e7:8000"

case "$action" in
  start-b|iterate-b|rollback-b|status) ;;
  *) echo "usage: $0 {start-b|iterate-b|rollback-b|status}" >&2; exit 2 ;;
esac

exec 9>"$lock"
flock --nonblock 9 || { echo "node06 deployment lock is held" >&2; exit 1; }

health_count() {
  curl -fsS http://127.0.0.1:8006/health | python3 -c '
import json, sys
value = json.load(sys.stdin)
print("{}/{}".format(value.get("healthy_replicas", -1), value.get("total_replicas", -1)))
'
}

wait_http() {
  local url=$1
  local attempts=$2
  local delay=$3
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
      return 0
    fi
    sleep "$delay"
  done
  return 1
}

wait_lb_count() {
  local expected=$1
  local attempts=$2
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if [[ $(health_count 2>/dev/null || true) == "$expected" ]]; then
      return 0
    fi
    sleep 2
  done
  return 1
}

check_intake() {
  local value
  value=$(curl -fsS --max-time 2 http://127.0.0.1:9100/metrics |
    sed -n 's/^node_ipmi_temperature_celsius{sensor="FP_TEMP"} //p')
  [[ $value =~ ^[0-9]+([.][0-9]+)?$ ]] || {
    echo "intake-air telemetry is unavailable" >&2
    return 1
  }
  python3 - "$value" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) < 55.0 else 1)
PY
}

validate_inputs() {
  [[ $(sha256sum "$old_dir/docker-compose.yaml" | cut -d' ' -f1) == "$old_compose_sha256" ]] || {
    echo "current Qwen3.8-27B Compose bytes changed" >&2
    return 1
  }
  [[ $(sha256sum "$new_dir/docker-compose.yaml" | cut -d' ' -f1) == "$new_compose_sha256" ]] || {
    echo "Qwen3.8-Flash-Next Compose bytes changed" >&2
    return 1
  }
  [[ $(docker inspect ds4-loadbalancer --format '{{index .Config.Labels "com.docker.compose.project.config_files"}}') == "$old_dir/docker-compose.yaml" ]] || {
    echo "load balancer was not created from the admitted one-file baseline" >&2
    return 1
  }
  [[ $(docker inspect ds4-loadbalancer --format '{{.Image}}') == "sha256:${lb_image##*@sha256:}" ]] || {
    echo "load-balancer image changed" >&2
    return 1
  }
  [[ -f "$model_dir/config.json" && -f "$model_dir/model.safetensors.index.json" ]] || {
    echo "immutable candidate model directory is incomplete" >&2
    return 1
  }
  docker image inspect "$candidate_image" >/dev/null
  (cd "$new_dir" && python3 ./validate-compose.py >/dev/null)
  check_intake
}

single_home() {
  (cd "$old_dir" && env LB_IMAGE="$lb_image" RJ_UPSTREAM="$single_upstreams" \
    docker compose up -d --no-deps ds4-loadbalancer)
  wait_lb_count 4/4 30 || { echo "single-home LB did not reach 4/4" >&2; return 1; }
}

stop_old_b() {
  (cd "$old_dir" && docker compose stop \
    qwen38-sg-e4 qwen38-sg-e5 qwen38-sg-e6 qwen38-sg-e7)
}

stop_candidate() {
  (cd "$new_dir" && docker compose stop qwen38flashnext-b >/dev/null 2>&1) || true
}

restore_baseline() {
  set +e
  stop_candidate
  cd "$old_dir" || return 1
  docker compose start qwen38-sg-e4 qwen38-sg-e5 qwen38-sg-e6 qwen38-sg-e7 || \
    docker compose up -d --no-deps qwen38-sg-e4 qwen38-sg-e5 qwen38-sg-e6 qwen38-sg-e7
  wait_http http://127.0.0.1:8034/health 240 5 || return 1
  wait_http http://127.0.0.1:8035/health 240 5 || return 1
  wait_http http://127.0.0.1:8036/health 240 5 || return 1
  wait_http http://127.0.0.1:8037/health 240 5 || return 1
  env LB_IMAGE="$lb_image" RJ_UPSTREAM="$full_upstreams" \
    docker compose up -d --no-deps ds4-loadbalancer || return 1
  wait_lb_count 8/8 60 || return 1
  echo "baseline restored: 8/8 upstreams"
}

if [[ $action == status ]]; then
  printf 'lb=%s\n' "$(health_count)"
  docker ps --format '{{.Names}}|{{.Status}}' --filter name=qwen38flashnext-b
  exit 0
fi

if [[ $action == rollback-b ]]; then
  restore_baseline
  exit $?
fi

validate_inputs
if [[ $action == start-b ]]; then
  [[ $(health_count) == 8/8 ]] || { echo "baseline is not 8/8 healthy" >&2; exit 1; }
else
  [[ $(health_count) == 4/4 ]] || { echo "single-homed production is not 4/4 healthy" >&2; exit 1; }
fi

rollback_on_error() {
  local status=$?
  trap - ERR INT TERM
  if [[ $action == iterate-b ]]; then
    stop_candidate
    echo "candidate start failed; production remains single-homed at 4/4" >&2
    exit "$status"
  fi
  echo "canary start failed; restoring baseline" >&2
  if ! restore_baseline; then
    echo "automatic baseline restore failed; production remains single-homed" >&2
  fi
  exit "$status"
}
trap rollback_on_error ERR INT TERM

if [[ $action == start-b ]]; then
  single_home
fi
stop_old_b
check_intake
(cd "$new_dir" && env ENGINE_RESTART_POLICY=no \
  docker compose up -d --no-deps qwen38flashnext-b)

experiment_id=$(date -u +%Y%m%dT%H%M%SZ)-fp8-vllm-startup-b
experiment_dir="$new_dir/.experiments/$experiment_id"
install -d -o root -g root -m 0700 "$experiment_dir"
telemetry="$experiment_dir/startup-telemetry.csv"
install -o root -g root -m 0600 /dev/null "$telemetry"

ready=0
for ((attempt = 1; attempt <= 240; attempt++)); do
  check_intake
  timestamp=$(date -u +%FT%TZ)
  nvidia-smi \
    --query-gpu=index,temperature.gpu,power.draw,power.limit,utilization.gpu,utilization.memory,memory.used,memory.total \
    --format=csv,noheader,nounits |
    sed "s/^/$timestamp,/" >>"$telemetry"
  state=$(docker inspect qwen38flashnext-b --format '{{.State.Status}}')
  [[ $state == running ]] || { echo "candidate container exited" >&2; false; }
  if curl -fsS --max-time 2 http://127.0.0.1:8041/health >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 5
done
[[ $ready == 1 ]] || { echo "candidate readiness timed out" >&2; false; }

trap - ERR INT TERM
echo "candidate B ready; production remains single-homed at 4/4; evidence=$experiment_dir"
