#!/usr/bin/env bash
# Start the pinned NVIDIA NVFP4 checkpoint as an isolated engine-B canary.
set -Eeuo pipefail

deployment=/home/luke/inference/qwen38_flash_next
experiment=$deployment/.experiments/20260907T1559Z-nvidia-nvfp4-tp4
candidate_compose=$experiment/docker-compose.candidate.yaml
baseline_compose=$experiment/docker-compose.baseline.yaml
candidate_argv=$experiment/candidate-argv.json
lock_file=/run/lock/ramjet-node06-deployment.lock
engine=qwen38flashnext-b
peer=qwen38flashnext-a
candidate_image_id=sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
baseline_image_id=sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3
candidate_revision=fc694b54fb0174e0913e6adf86691ef85a4ead47
candidate_compose_sha=b618c4238f86e8aa1b17278a6b9bb792bd2bc06931df1956767b246ee5698889
baseline_compose_sha=e59dd53e2456dc3e07810041b72b0b521111f9e46e18954c6dc6ee6bd2dffc9d
candidate_argv_sha=5258766127f765f9fae9c136191a5e5a468a1e6669423ad81d80e8123b769961
model_manifest_sha=4626ab3546e271ac6b9872b6af02bfddd0d39ccb91cb6a30fe25a9ed53297c31
state_file=$experiment/canary-rollout-state.txt
log_file=$experiment/canary-engine.log
metrics_file=$experiment/canary-metrics.txt
candidate_started=0
ready=0

fail() {
  echo "NVIDIA NVFP4 canary: $*" >&2
  exit 2
}

compose() {
  env ENGINE_RESTART_POLICY=no docker compose \
    --project-directory "$deployment" -f "$candidate_compose" "$@"
}

intake_c() {
  curl -fsS --max-time 5 http://127.0.0.1:9100/metrics |
    awk '/^node_ipmi_temperature_celsius[{]/ && /sensor="FP_TEMP"/ {print $NF}'
}

rollback() {
  local status=$?
  trap - EXIT INT TERM
  if ((candidate_started == 1 && ready == 0)); then
    docker update --restart=no "$engine" >/dev/null 2>&1 || true
    docker stop --time 30 "$engine" >/dev/null 2>&1 || true
  fi
  if ((status != 0)); then
    {
      date -u +%FT%TZ
      echo "result=failed"
      echo "exit_code=$status"
    } >>"$state_file"
  fi
  exit "$status"
}
trap rollback EXIT INT TERM

[[ $(hostname) == node06 ]] || fail "this canary may run only on node06"
[[ $(id -u) == 0 ]] || fail "this canary must run as root"
[[ $(stat -c '%u:%a' "$experiment") == 0:700 ]] ||
  fail "experiment directory authority changed"
[[ $(sha256sum "$candidate_compose" | awk '{print $1}') == "$candidate_compose_sha" ]] ||
  fail "candidate Compose changed"
[[ $(sha256sum "$baseline_compose" | awk '{print $1}') == "$baseline_compose_sha" ]] ||
  fail "baseline rollback Compose changed"
[[ $(sha256sum "$candidate_argv" | awk '{print $1}') == "$candidate_argv_sha" ]] ||
  fail "candidate argv changed"
[[ $(sha256sum "$experiment/model-verification.json" | awk '{print $1}') == "$model_manifest_sha" ]] ||
  fail "model verification changed"
[[ ! -e $state_file && ! -e $log_file && ! -e $metrics_file ]] ||
  fail "canary output already exists"

start_air=$(intake_c)
[[ $start_air =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "intake telemetry is invalid"
awk -v temperature="$start_air" 'BEGIN {exit !(temperature <= 46)}' ||
  fail "intake air is above the 46C launch threshold"

set -a
# shellcheck disable=SC1091
source "$deployment/.env"
set +a
VLLM_API_KEY=${VLLM_API_KEY:-}
[[ ${#VLLM_API_KEY} -ge 16 ]] || fail "engine bearer authority is invalid"

exec 9>"$lock_file"
flock -n 9 || fail "another node06 deployment operation owns the lock"

docker inspect "$peer" | jq -e --arg image "$baseline_image_id" '
  length == 1 and .[0].Image == $image and .[0].State.Status == "running" and
  .[0].State.OOMKilled == false and .[0].RestartCount == 0
' >/dev/null || fail "the serving FP8 peer is not the pinned healthy process"
curl -fsS --max-time 5 http://127.0.0.1:8006/health | jq -e '
  .status == "ok" and .healthy_replicas == 1 and .active_replicas == 1
' >/dev/null || fail "ramjet is not safely single-homed on the peer"

for gpu in 4 5 6 7; do
  used=$(nvidia-smi --id="$gpu" --query-gpu=memory.used --format=csv,noheader,nounits)
  [[ $used =~ ^[[:space:]]*[0-9]+[[:space:]]*$ ]] || fail "GPU memory telemetry is invalid"
  ((used < 1024)) || fail "candidate GPU $gpu is already occupied"
done

umask 077
{
  date -u +%FT%TZ
  echo "result=starting"
  echo "intake_c=$start_air"
  echo "candidate_image_id=$candidate_image_id"
  echo "candidate_revision=$candidate_revision"
} >"$state_file"

compose up -d --no-deps --force-recreate "$engine"
candidate_started=1
started_at=$(date -u +%FT%TZ)
deadline=$((SECONDS + 1200))
next_checkpoint=$SECONDS

while true; do
  air=$(intake_c)
  [[ $air =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "intake telemetry disappeared"
  awk -v temperature="$air" 'BEGIN {exit !(temperature < 50)}' ||
    fail "intake air reached the 50C stop threshold"

  status=$(docker inspect --format '{{.State.Status}}' "$engine") ||
    fail "candidate container disappeared"
  [[ $status == running ]] || fail "candidate container stopped before readiness"

  if curl -fsS --max-time 5 http://127.0.0.1:8041/health >/dev/null 2>&1; then
    break
  fi
  ((SECONDS < deadline)) || fail "candidate readiness timed out"
  if ((SECONDS >= next_checkpoint)); then
    gpu_summary=$(nvidia-smi --query-gpu=index,temperature.gpu,memory.used,utilization.gpu \
      --format=csv,noheader,nounits | awk -F, '$1 + 0 >= 4 {gsub(/ /, ""); printf "%s:%s:%s:%s%s", $1, $2, $3, $4, NR == 8 ? "" : ","}')
    printf '%s intake_c=%s candidate_gpus=%s\n' "$(date -u +%FT%TZ)" "$air" "$gpu_summary" >>"$state_file"
    next_checkpoint=$((SECONDS + 30))
  fi
  sleep 5
done

docker inspect "$engine" | jq -e \
  --arg image "$candidate_image_id" --arg revision "$candidate_revision" '
  length == 1 and .[0].Image == $image and .[0].State.Status == "running" and
  .[0].State.OOMKilled == false and .[0].RestartCount == 0 and
  .[0].HostConfig.RestartPolicy.Name == "no" and
  .[0].Config.Labels["ai.ramjet.model.repository"] == "nvidia/Qwen3.8-Flash-Next-NVFP4" and
  .[0].Config.Labels["ai.ramjet.model.revision"] == $revision
' >/dev/null || fail "live candidate identity differs from the admitted render"
docker inspect --format '{{json .Config.Cmd}}' "$engine" |
  jq -e --slurpfile expected "$candidate_argv" '. == $expected[0]' >/dev/null ||
  fail "live candidate argv differs from the admitted render"

curl -fsS --max-time 10 -H "Authorization: Bearer $VLLM_API_KEY" \
  http://127.0.0.1:8041/metrics |
  grep -E '^(vllm:cache_config_info|vllm:num_requests_running|vllm:num_requests_waiting)' \
  >"$metrics_file"
docker logs --since "$started_at" --tail 2000 "$engine" >"$log_file" 2>&1
if grep -Eiq 'Traceback|CUDA error|out of memory|NCCL.*(error|fail)|Xid|fatal|panic' "$log_file"; then
  fail "candidate logs contain a fatal runtime marker"
fi

ready=1
final_air=$(intake_c)
{
  date -u +%FT%TZ
  echo "result=ready"
  echo "intake_c=$final_air"
} >>"$state_file"
echo "qwen_nvidia_nvfp4_canary=ready"
