#!/usr/bin/env bash
# Start pinned Kev-small, qualify a four-upstream Ramjet canary, then roll only
# the stateless public Ramjet container. Qwen and GLM lifecycles are untouched.
set -Eeuo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
compose="$root/docker-compose.yaml"
secret_env=${RAMJET_SECRET_ENV:-/home/luke/inference/qwen38_flash_next/.env}
lock=${RAMJET_DEPLOYMENT_LOCK:-/run/lock/ramjet-node06-deployment.lock}
lb_image=${LB_IMAGE:?set LB_IMAGE to an immutable tag@sha256 reference}
kev_image=${KEV_IMAGE:?set KEV_IMAGE to an immutable tag@sha256 reference}
for image in "$lb_image" "$kev_image"; do
  [[ "$image" == ghcr.io/helixml/*:*@sha256:* ]] || {
    echo "images must be immutable GHCR tag@sha256 references" >&2
    exit 2
  }
done
[[ -f "$secret_env" && ! -L "$secret_env" && $(stat -c %a "$secret_env") == 600 ]] || {
  echo "protected mode-0600 secret environment is required" >&2
  exit 2
}

exec 9>"$lock"
flock -n 9 || {
  echo "deployment lock is busy" >&2
  exit 75
}

for network in qwen38_flash_next_default glm53_flash_sm120_default qwen38_27b_default; do
  docker network inspect "$network" >/dev/null
done
for engine in qwen38flashnext-a glm53sm120-b glm53sm120-c; do
  [[ $(docker inspect -f '{{.State.Running}} {{.RestartCount}}' "$engine") == "true 0" ]] || {
    echo "engine preflight failed: $engine" >&2
    exit 1
  }
done
runtime_project=qwen38_glm53_kev_runtime
candidate_project=qwen38_glm53_kev_canary
candidate_name=ds4-loadbalancer-systemone-canary
rollout_stamp=$(date -u +%Y%m%dT%H%M%SZ)
rollback_name="ds4-loadbalancer-rollback-$rollout_stamp"
canonical_project="qwen38_glm53_kev_release_${rollout_stamp,,}_$$"
kev_started=0
promoted=0
network_created=0

compose_run() {
  env LB_IMAGE="$lb_image" KEV_IMAGE="$kev_image" \
    docker compose --env-file "$secret_env" -f "$compose" "$@"
}

cleanup_candidate() {
  env LB_IMAGE="$lb_image" KEV_IMAGE="$kev_image" \
    LB_CONTAINER_NAME="$candidate_name" LB_API_PORT=18006 \
    LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
    docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
      down --remove-orphans >/dev/null 2>&1 || true
}

rollback() {
  docker logs --tail 100 kev-small >&2 2>/dev/null || true
  docker logs --tail 100 "$candidate_name" >&2 2>/dev/null || true
  cleanup_candidate
  if [[ $promoted == 1 ]]; then
    compose_run -p "$canonical_project" down --remove-orphans >/dev/null 2>&1 || true
    if docker inspect "$rollback_name" >/dev/null 2>&1; then
      docker rename "$rollback_name" ds4-loadbalancer
      docker start ds4-loadbalancer >/dev/null
    fi
  fi
  if [[ $kev_started == 1 ]]; then
    compose_run -p "$runtime_project" down --remove-orphans >/dev/null 2>&1 || true
  fi
  if [[ $network_created == 1 ]]; then
    docker network rm ramjet_kev_systemone >/dev/null 2>&1 || true
  fi
}
trap rollback ERR
trap 'rollback; exit 130' INT
trap 'rollback; exit 143' TERM

if ! docker network inspect ramjet_kev_systemone >/dev/null 2>&1; then
  docker network create --internal ramjet_kev_systemone >/dev/null
  network_created=1
fi
[[ $(docker network inspect ramjet_kev_systemone -f '{{.Internal}}') == true ]] || {
  echo "ramjet_kev_systemone must be an internal Docker network" >&2
  exit 1
}

scratch=$(mktemp -d)
chmod 0700 "$scratch"
trap 'rm -rf -- "$scratch"' EXIT
authorization_header="$scratch/authorization.header"
python3 - "$secret_env" "$authorization_header" <<'PY'
import os, shlex, sys
source, destination = sys.argv[1:]
value = None
with open(source, encoding="utf-8") as env_file:
    for raw_line in env_file:
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, raw_value = line.split("=", 1)
        if key.strip() == "VLLM_API_KEY":
            parsed = shlex.split(raw_value, comments=True, posix=True)
            if len(parsed) != 1:
                raise SystemExit("VLLM_API_KEY must be one dotenv value")
            value = parsed[0]
            break
if not value or "\r" in value or "\n" in value:
    raise SystemExit("protected environment has no valid VLLM_API_KEY")
descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(descriptor, "w", encoding="utf-8") as output:
    output.write(f"Authorization: Bearer {value}\n")
PY

systemone_body='{"state":"The invoice was charged twice.","model":"kev-latest","questions":{"billing":{"type":"noul","instructions":"Is this about billing?"}}}'
warmup_body='{"state":"Runtime warmup input.","model":"kev-latest","questions":{"ready":{"type":"noul","instructions":"Is the runtime ready?"}}}'

validate_endpoint() {
  local port=$1
  local evidence=$2
  install -d -m 0700 "$evidence"
  local ready=0
  for _attempt in $(seq 1 90); do
    if curl -fsS --max-time 2 "http://127.0.0.1:${port}/health" >"$evidence/health.json" 2>/dev/null \
      && curl -fsS --max-time 5 "http://127.0.0.1:${port}/v1/models" >"$evidence/models.json" 2>/dev/null \
      && python3 - "$evidence/health.json" "$evidence/models.json" <<'PY'
import json, sys
health = json.load(open(sys.argv[1], encoding="utf-8"))
models = json.load(open(sys.argv[2], encoding="utf-8"))
assert health["healthy_replicas"] == 4, health
assert health["total_replicas"] == 4, health
assert {item["id"] for item in models["data"]} == {
    "qwen3.8-flash-next", "glm-5.3-flash"
}, models
PY
    then
      ready=1
      break
    fi
    sleep 1
  done
  [[ $ready == 1 ]] || {
    echo "four-upstream Ramjet did not become ready on port $port" >&2
    return 1
  }

  curl -fsS --max-time 60 -D "$evidence/kev.headers" \
    -H @"$authorization_header" -H 'content-type: application/json' \
    --data "$systemone_body" \
    "http://127.0.0.1:${port}/v1/systemone" >"$evidence/kev.json"
  python3 - "$evidence/kev.json" "$evidence/kev.headers" <<'PY'
import json, sys
response = json.load(open(sys.argv[1], encoding="utf-8"))
assert response["model"] == "kev-latest", response
assert response["answers"]["billing"]["type"] == "noul", response
headers = open(sys.argv[2], encoding="utf-8").read().lower().replace("\r\n", "\n")
assert "x-ramjet-upstream: 3\n" in headers, headers
PY

  curl -fsS --max-time 60 -D "$evidence/qwen.headers" \
    -H @"$authorization_header" -H 'content-type: application/json' \
    --data '{"model":"qwen3.8-flash-next","messages":[{"role":"user","content":"Reply with one word."}],"max_tokens":8,"temperature":0}' \
    "http://127.0.0.1:${port}/v1/chat/completions" >"$evidence/qwen.json"
  python3 - "$evidence/qwen.json" "$evidence/qwen.headers" <<'PY'
import json, sys
response = json.load(open(sys.argv[1], encoding="utf-8"))
assert response.get("choices") and "error" not in response, response
headers = open(sys.argv[2], encoding="utf-8").read().lower().replace("\r\n", "\n")
assert "x-ramjet-upstream: 0\n" in headers, headers
PY

  [[ $(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 \
    -H @"$authorization_header" -H 'content-type: application/json' \
    --data '{"model":"kev-latest","messages":[]}' \
    "http://127.0.0.1:${port}/v1/chat/completions") == 404 ]]
  [[ $(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 \
    -H @"$authorization_header" -H 'content-type: application/json' \
    --data '{"state":"x","model":"qwen3.8-flash-next","questions":{"q":{"type":"noul","instructions":"x"}}}' \
    "http://127.0.0.1:${port}/v1/systemone") == 404 ]]
}

compose_run -p "$runtime_project" config --quiet
compose_run -p "$runtime_project" up -d --no-deps kev-small
kev_started=1
for _attempt in $(seq 1 180); do
  [[ $(docker inspect -f '{{.State.Health.Status}}' kev-small 2>/dev/null) == healthy ]] && break
  sleep 2
done
[[ $(docker inspect -f '{{.State.Health.Status}}' kev-small) == healthy ]] || {
  docker logs --tail 100 kev-small >&2
  exit 1
}

# Readiness proves that weights loaded, but the first Qwen3.5 request also
# compiles the FLA/Triton inference kernels. Pay that one-time cost directly
# before Ramjet's client and public latency budgets are involved, then use a
# different state in the canary to prove ordinary uncached inference.
printf '%s' "$warmup_body" | docker exec -i kev-small python -c '
import sys, urllib.request
body = sys.stdin.buffer.read()
request = urllib.request.Request(
    "http://127.0.0.1:8009/v1/systemone",
    data=body,
    headers={"content-type": "application/json"},
)
sys.stdout.buffer.write(urllib.request.urlopen(request, timeout=300).read())
' >"$scratch/kev-warmup.json"
python3 - "$scratch/kev-warmup.json" <<'PY'
import json, sys
response = json.load(open(sys.argv[1], encoding="utf-8"))
assert response["model"] == "kev-latest", response
assert response["answers"]["ready"]["type"] == "noul", response
PY

env LB_IMAGE="$lb_image" KEV_IMAGE="$kev_image" \
  LB_CONTAINER_NAME="$candidate_name" LB_API_PORT=18006 \
  LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
  docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
    up -d --no-deps ds4-loadbalancer
validate_endpoint 18006 "$scratch/candidate"
cleanup_candidate

docker stop -t 30 ds4-loadbalancer >/dev/null
docker rename ds4-loadbalancer "$rollback_name"
promoted=1
compose_run -p "$canonical_project" up -d --no-deps ds4-loadbalancer
validate_endpoint 8006 "$scratch/promoted"

promoted=0
kev_started=0
network_created=0
trap - ERR INT TERM
printf 'Qwen/GLM/Kev Ramjet promoted; exact stopped rollback container=%s\n' "$rollback_name"
