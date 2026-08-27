#!/usr/bin/env bash
# One guarded, MTP-off TP4 startup/correctness/brief-concurrency sequence.
set -euo pipefail

readonly SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
readonly COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yaml"
readonly LOCK_FILE=/run/lock/ramjet-node06-deployment.lock
readonly BASE_URL=http://127.0.0.1:8051
readonly METRICS_URL=$BASE_URL/metrics
readonly TEST_MAX_CONCURRENCY=${GLM_TEST_MAX_CONCURRENCY:-24}
: "${EXPERIMENT_DIR:?set EXPERIMENT_DIR to a fresh mode-0700 directory}"

case $TEST_MAX_CONCURRENCY in
  1|8|16|24) ;;
  *) echo "GLM_TEST_MAX_CONCURRENCY must be 1, 8, 16, or 24" >&2; exit 2 ;;
esac

[[ -d $EXPERIMENT_DIR && ! -L $EXPERIMENT_DIR ]] || {
  echo "EXPERIMENT_DIR must be a real directory" >&2
  exit 2
}
[[ $(stat -c %a "$EXPERIMENT_DIR") == 700 ]] || {
  echo "EXPERIMENT_DIR must have mode 0700" >&2
  exit 2
}
[[ -f $SCRIPT_DIR/.env && $(stat -c %a "$SCRIPT_DIR/.env") == 600 ]] || {
  echo "missing mode-0600 deployment environment" >&2
  exit 2
}

compose() {
  docker compose --env-file "$SCRIPT_DIR/.env" -f "$COMPOSE_FILE" "$@"
}

is_running() {
  [[ $(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true) == true ]]
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  flock -u 9 2>/dev/null || true
  if is_running glm53-b; then
    exec 8>"$LOCK_FILE"
    if flock -w 30 8; then
      compose stop -t 90 glm53-b || true
      flock -u 8 || true
    fi
  fi
  docker logs glm53-b >"$EXPERIMENT_DIR/glm53-b.log" 2>&1 || true
  docker inspect glm53-b >"$EXPERIMENT_DIR/glm53-b.inspect.json" 2>&1 || true
  nvidia-smi --query-gpu=index,temperature.gpu,memory.used,utilization.gpu,power.draw \
    --format=csv,noheader,nounits >"$EXPERIMENT_DIR/final-gpus.csv" 2>&1 || true
  exit "$status"
}
trap cleanup EXIT INT TERM

air_temperature() {
  curl -fsS http://127.0.0.1:9100/metrics | awk '
    /node_ipmi_temperature_celsius\{/ &&
    ($0 ~ /sensor="FP_TEMP"/ || $0 ~ /sensor="Inlet Temp"/) {
      if (!seen || $NF > hottest) hottest=$NF
      seen=1
    }
    END { if (!seen) exit 1; print hottest }
  '
}

max_gpu_temperature() {
  nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader,nounits |
    awk 'BEGIN { max=-999 } { if ($1 > max) max=$1 } END { print max }'
}

thermal_snapshot() {
  local label=$1
  printf '%s intake_air_c=%s max_gpu_c=%s\n' \
    "$label" "$(air_temperature)" "$(max_gpu_temperature)" |
    tee -a "$EXPERIMENT_DIR/thermal-checkpoints.txt"
}

run_scout() {
  local concurrency=$1
  local max_tokens=$2
  thermal_snapshot "before-c${concurrency}"
  python3 "$SCRIPT_DIR/brief-scout.py" "$BASE_URL" \
    --metrics "$METRICS_URL" \
    --concurrency "$concurrency" \
    --max-tokens "$max_tokens" |
    tee "$EXPERIMENT_DIR/c${concurrency}.json"
}

exec 9>"$LOCK_FILE"
flock -w 30 9
for peer in glm53-a ds4-loadbalancer; do
  if is_running "$peer"; then
    echo "refusing to test while $peer is running" >&2
    exit 1
  fi
done
python3 "$SCRIPT_DIR/verify-model.py" \
  /prod/models/LibertAIDAI/GLM-5.3-Flash-NVFP4-9e0d74e3cef1
# The validator proves the checked-in safe baseline. An explicitly selected
# experiment variant is rendered by the following Compose check and retained
# in the container environment/inspect evidence.
env GLM_MTP_MODE=off python3 "$SCRIPT_DIR/validate-compose.py"
compose config --quiet
compose up -d --no-deps glm53-b

ready_deadline=$((SECONDS + 600))
until curl -fsS --max-time 3 "$BASE_URL/health" >/dev/null; do
  if ! is_running glm53-b; then
    docker inspect -f '{{json .State}}' glm53-b >&2 || true
    exit 1
  fi
  if (( SECONDS >= ready_deadline )); then
    echo "glm53-b did not become healthy within 600 seconds" >&2
    exit 1
  fi
  sleep 5
done
docker inspect -f '{{json .State}}' glm53-b >"$EXPERIMENT_DIR/ready-state.json"
flock -u 9

thermal_snapshot ready
python3 "$SCRIPT_DIR/smoke.py" "$BASE_URL" --metrics "$METRICS_URL" |
  tee "$EXPERIMENT_DIR/smoke.json"
run_scout 1 128
if (( TEST_MAX_CONCURRENCY >= 8 )); then
  run_scout 8 64
fi
if (( TEST_MAX_CONCURRENCY >= 16 )); then
  run_scout 16 64
fi

if (( TEST_MAX_CONCURRENCY >= 24 )); then
  intake=$(air_temperature)
  gpu_c=$(max_gpu_temperature)
  if awk -v air="$intake" -v gpu="$gpu_c" 'BEGIN { exit !(air <= 44 && gpu <= 70) }'; then
    run_scout 24 64
  else
    printf '{"ok":true,"skipped":true,"reason":"thermal_headroom","intake_air_c":%s,"max_gpu_c":%s}\n' \
      "$intake" "$gpu_c" | tee "$EXPERIMENT_DIR/c24.json"
  fi
fi

thermal_snapshot complete
exec 9>"$LOCK_FILE"
flock -w 30 9
compose stop -t 90 glm53-b
flock -u 9
trap - EXIT INT TERM
docker logs glm53-b >"$EXPERIMENT_DIR/glm53-b.log" 2>&1
docker inspect glm53-b >"$EXPERIMENT_DIR/glm53-b.inspect.json"
nvidia-smi --query-gpu=index,temperature.gpu,memory.used,utilization.gpu,power.draw \
  --format=csv,noheader,nounits >"$EXPERIMENT_DIR/final-gpus.csv"
