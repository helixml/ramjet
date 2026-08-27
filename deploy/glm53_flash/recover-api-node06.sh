#!/usr/bin/env bash
# Recover the qualified single-engine GLM API behind Ramjet and node06 Caddy.
# Run this script only as the child of bench/node06_gpu_guard.py.
set -euo pipefail

readonly SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
readonly COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yaml"
readonly LOCK_FILE=/run/lock/ramjet-node06-deployment.lock
readonly ENGINE_URL=http://127.0.0.1:8051
readonly RAMJET_URL=http://127.0.0.1:8006
readonly CADDY_URL=http://127.0.0.1
readonly SERVED_MODEL=glm-5.3-flash
: "${EXPERIMENT_DIR:?set EXPERIMENT_DIR to a fresh mode-0700 directory}"

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

exec >>"$EXPERIMENT_DIR/recovery.log" 2>&1
echo "stage=preflight"

compose() {
  docker compose --env-file "$SCRIPT_DIR/.env" -f "$COMPOSE_FILE" "$@"
}

is_running() {
  [[ $(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true) == true ]]
}

models_body=

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n $models_body ]]; then
    rm -f "$models_body"
  fi
  if (( status != 0 )); then
    compose stop -t 30 ds4-loadbalancer >/dev/null 2>&1 || true
    compose stop -t 90 glm53-b >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

exec 9>"$LOCK_FILE"
flock -w 30 9
echo "stage=locked"

if is_running glm53-a; then
  echo "refusing recovery while glm53-a is running" >&2
  exit 1
fi

python3 "$SCRIPT_DIR/verify-model.py" \
  /prod/models/LibertAIDAI/GLM-5.3-Flash-NVFP4-9e0d74e3cef1 >/dev/null
python3 "$SCRIPT_DIR/validate-compose.py" >/dev/null
export RJ_UPSTREAM=http://glm53-b:8000
compose config --quiet
echo "stage=start_engine"
compose up -d --no-deps glm53-b

ready_deadline=$((SECONDS + 900))
until curl -fsS --max-time 3 "$ENGINE_URL/health" >/dev/null 2>&1; do
  if ! is_running glm53-b; then
    echo "glm53-b exited before readiness" >&2
    docker inspect -f '{{json .State}}' glm53-b >&2 || true
    exit 1
  fi
  if (( SECONDS >= ready_deadline )); then
    echo "glm53-b readiness timeout" >&2
    exit 1
  fi
  sleep 5
done
echo "direct_engine=healthy"

echo "stage=start_ramjet"
compose up -d --no-deps ds4-loadbalancer
ramjet_deadline=$((SECONDS + 90))
until curl -fsS --max-time 3 "$RAMJET_URL/health" >"$EXPERIMENT_DIR/ramjet-health.json" 2>/dev/null; do
  if ! is_running ds4-loadbalancer; then
    echo "Ramjet exited before readiness" >&2
    exit 1
  fi
  if (( SECONDS >= ramjet_deadline )); then
    echo "Ramjet readiness timeout" >&2
    exit 1
  fi
  sleep 2
done

models_body=$(mktemp "$EXPERIMENT_DIR/models.XXXXXX")

curl -fsS --max-time 15 "$RAMJET_URL/v1/models" >"$models_body"
python3 -c '
import json, sys
payload = json.load(open(sys.argv[1]))
expected = sys.argv[2]
assert expected in {item.get("id") for item in payload.get("data", [])}
' "$models_body" "$SERVED_MODEL"

caddy_token=$(grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile | head -1 | cut -d' ' -f2)
[[ -n $caddy_token ]] || {
  echo "Caddy bearer authority is missing" >&2
  exit 1
}
curl -fsS --max-time 15 \
  -H "Authorization: Bearer $caddy_token" \
  "$CADDY_URL/v1/models" >"$models_body"
python3 -c '
import json, sys
payload = json.load(open(sys.argv[1]))
expected = sys.argv[2]
assert expected in {item.get("id") for item in payload.get("data", [])}
' "$models_body" "$SERVED_MODEL"

# Reuse the installed one-token full-path probe instead of injecting another
# request while clients may already be filling the newly available engine.
systemctl start ds4-synthetic-probe.service
python3 -c '
import pathlib, re
text = pathlib.Path("/var/lib/prometheus/node-exporter/ds4_synthetic_probe.prom").read_text()
def value(name):
    match = re.search(rf"^{name} ([0-9.]+)$", text, re.MULTILINE)
    assert match, name
    return float(match.group(1))
assert value("ds4_synthetic_probe_success") == 1
assert value("ds4_synthetic_probe_http_code") == 200
'
echo "caddy_inference=passed"

curl -fsS --max-time 10 http://127.0.0.1:8007/metrics |
  awk '/^ramjet_upstream_up/ { seen=1; if ($NF != 1) exit 1 } END { exit !seen }'

docker inspect -f '{{json .State}}' glm53-b >"$EXPERIMENT_DIR/glm53-b-state.json"
docker inspect -f '{{json .State}}' ds4-loadbalancer >"$EXPERIMENT_DIR/ramjet-state.json"
nvidia-smi --query-gpu=index,temperature.gpu,memory.used,utilization.gpu,power.draw \
  --format=csv,noheader,nounits >"$EXPERIMENT_DIR/final-gpus.csv"

rm -f "$models_body"
models_body=
trap - EXIT INT TERM
echo "api_recovery=passed"
