#!/usr/bin/env bash
# Roll only Ramjet from the existing homogeneous Qwen deployment to the
# static Qwen/GLM multi-model deployment. Run through node06_gpu_guard.py.
set -Eeuo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
compose="$root/docker-compose.yaml"
secret_env=${RAMJET_SECRET_ENV:-/home/luke/inference/qwen38_flash_next/.env}
lock=${RAMJET_DEPLOYMENT_LOCK:-/run/lock/ramjet-node06-deployment.lock}
image=${LB_IMAGE:?set LB_IMAGE to an immutable tag@sha256 reference}
canary_only=${RAMJET_CANARY_ONLY:-0}
case "$image" in
  ghcr.io/helixml/ramjet:*@sha256:*) ;;
  *) echo "LB_IMAGE must be an immutable GHCR tag@sha256 reference" >&2; exit 2 ;;
esac
[[ "$canary_only" == 0 || "$canary_only" == 1 ]] || {
  echo "RAMJET_CANARY_ONLY must be 0 or 1" >&2
  exit 2
}
[[ -f "$secret_env" && ! -L "$secret_env" ]] || {
  echo "missing protected secret environment" >&2
  exit 2
}
[[ $(stat -c %a "$secret_env") == 600 ]] || {
  echo "secret environment must have mode 0600" >&2
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
for engine in qwen38flashnext-a glm53sm120-b; do
  [[ $(docker inspect -f '{{.State.Running}} {{.RestartCount}}' "$engine") == "true 0" ]] || {
    echo "engine preflight failed: $engine" >&2
    exit 1
  }
done

candidate_project=qwen38_glm53_multimodel_canary
candidate_name=ds4-loadbalancer-multimodel-canary
rollout_stamp=$(date -u +%Y%m%dT%H%M%SZ)
rollback_name="ds4-loadbalancer-rollback-$rollout_stamp"
# Compose v5 discovers a renamed container by its immutable project/service
# labels and will otherwise adopt and delete it during `up`. A release-unique
# project keeps the preserved rollback container outside the candidate's
# reconciliation scope. The canonical public container name remains stable.
canonical_project="qwen38_glm53_multimodel_release_${rollout_stamp,,}_$$"
promoted=0

compose_run() {
  env LB_IMAGE="$image" docker compose --env-file "$secret_env" -f "$compose" "$@"
}

cleanup() {
  env LB_IMAGE="$image" LB_CONTAINER_NAME="$candidate_name" \
    LB_API_PORT=18006 LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
    docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
      down --remove-orphans >/dev/null 2>&1 || true
  if [[ $promoted == 1 ]]; then
    compose_run -p "$canonical_project" down --remove-orphans >/dev/null 2>&1 || true
    if docker inspect "$rollback_name" >/dev/null 2>&1; then
      docker rename "$rollback_name" ds4-loadbalancer
      docker start ds4-loadbalancer >/dev/null
    fi
  fi
}
trap cleanup ERR
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

validate_endpoint() {
  local port=$1
  local scratch=$2
  install -d -m 0700 "$scratch"
  local ready=0
  for _attempt in $(seq 1 60); do
    if curl -fsS --max-time 2 "http://127.0.0.1:${port}/health" \
        >"$scratch/health.json" 2>/dev/null \
      && curl -fsS --max-time 5 "http://127.0.0.1:${port}/v1/models" \
        >"$scratch/models.json" 2>/dev/null \
      && python3 - "$scratch/health.json" "$scratch/models.json" <<'PY'
import json, sys
health = json.load(open(sys.argv[1], encoding="utf-8"))
models = json.load(open(sys.argv[2], encoding="utf-8"))
assert health["healthy_replicas"] == 2, health
assert health["total_replicas"] == 2, health
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
  [[ "$ready" == 1 ]] || {
    echo "multi-model LB did not become ready on port $port" >&2
    return 1
  }
  for model in qwen3.8-flash-next glm-5.3-flash; do
    curl -fsS --max-time 60 \
      -H @"$authorization_header" \
      -H 'content-type: application/json' \
      --data "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply with one word.\"}],\"max_tokens\":8,\"temperature\":0}" \
      "http://127.0.0.1:${port}/v1/chat/completions" >"$scratch/${model}.json"
    python3 - "$scratch/${model}.json" <<'PY'
import json, sys
response = json.load(open(sys.argv[1], encoding="utf-8"))
assert response.get("choices"), "missing choices"
assert "error" not in response, "engine returned an error"
PY
  done
}

scratch=$(mktemp -d)
chmod 0700 "$scratch"
cleanup_scratch() {
  rm -rf -- "$scratch"
}
trap cleanup_scratch EXIT
authorization_header="$scratch/authorization.header"
python3 - "$secret_env" "$authorization_header" <<'PY'
import os
import shlex
import sys

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

# Prove the exact image and network contract on alternate loopback ports before
# interrupting the established stateless LB.
env LB_IMAGE="$image" LB_CONTAINER_NAME="$candidate_name" \
  LB_API_PORT=18006 LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
  docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
    config --quiet
env LB_IMAGE="$image" LB_CONTAINER_NAME="$candidate_name" \
  LB_API_PORT=18006 LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
  docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
    up -d --no-deps ds4-loadbalancer
validate_endpoint 18006 "$scratch/candidate"
env LB_IMAGE="$image" LB_CONTAINER_NAME="$candidate_name" \
  LB_API_PORT=18006 LB_METRICS_PORT=18007 LB_TAILSCALE_METRICS_PORT=18007 \
  docker compose --env-file "$secret_env" -p "$candidate_project" -f "$compose" \
    down --remove-orphans
if [[ "$canary_only" == 1 ]]; then
  trap - ERR INT TERM
  printf 'multi-model Ramjet alternate-port canary passed\n'
  exit 0
fi

# Preserve the exact old container as a stopped, instant rollback artifact.
docker stop -t 30 ds4-loadbalancer >/dev/null
docker rename ds4-loadbalancer "$rollback_name"
promoted=1
compose_run -p "$canonical_project" config --quiet
compose_run -p "$canonical_project" up -d --no-deps ds4-loadbalancer
validate_endpoint 8006 "$scratch/promoted"

promoted=0
trap - ERR INT TERM
printf 'multi-model Ramjet promoted; exact stopped rollback container=%s\n' "$rollback_name"
