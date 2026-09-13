#!/usr/bin/env bash
set -Eeuo pipefail

readonly qwen_dir=/home/luke/inference/qwen38_flash_next
readonly glm_dir=/home/luke/inference/glm53_flash_sm120
readonly lock_file=/run/lock/ramjet-node06-deployment.lock
[[ $(hostname) == node06 ]] || { echo "restore may run only on node06" >&2; exit 2; }
: "${EXPECTED_QWEN_COMPOSE_SHA256:?set exact operational Qwen Compose SHA-256}"
[[ $(sha256sum "$qwen_dir/docker-compose.yaml" | awk '{print $1}') == "$EXPECTED_QWEN_COMPOSE_SHA256" ]] || { echo "Qwen Compose bytes drifted" >&2; exit 2; }
exec 9>"$lock_file"
flock -n 9 || { echo "another node06 deployment operation owns the lock" >&2; exit 2; }

if [[ $(docker inspect --format '{{.State.Status}}' glm53sm120-c 2>/dev/null || true) == running ]]; then
  echo "refusing Qwen B restore while the production GLM C replica owns GPUs 6-7" >&2
  exit 2
fi

if [[ $(docker inspect --format '{{.State.Status}}' qwen38flashnext-b 2>/dev/null || true) == running ]] &&
  curl -fsS --max-time 5 http://127.0.0.1:8006/health |
    jq -e '.healthy_replicas >= 2 and .replicas[0].healthy == true and .replicas[1].healthy == true' >/dev/null; then
  echo "Qwen B is already restored; shared LB reports A and B healthy"
  exit 0
fi

docker compose -f "$glm_dir/docker-compose.yaml" --project-directory "$glm_dir" stop -t 120 glm53sm120-b
docker compose -f "$qwen_dir/docker-compose.yaml" --project-directory "$qwen_dir" up -d --no-deps --force-recreate qwen38flashnext-b
deadline=$((SECONDS + 900))
until curl -fsS --max-time 5 http://127.0.0.1:8006/health |
  jq -e '.healthy_replicas >= 2 and .replicas[0].healthy == true and .replicas[1].healthy == true' >/dev/null; do
  ((SECONDS < deadline)) || { echo "Qwen B restore timed out" >&2; exit 3; }
  sleep 3
done
echo "Qwen B restored; shared LB reports A and B healthy"
