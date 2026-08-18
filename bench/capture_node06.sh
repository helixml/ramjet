#!/usr/bin/env bash
# Capture the effective node06 serving configuration without printing secrets.
# Usage: bash bench/capture_node06.sh [ssh-host]
set -euo pipefail

target=${1:-node06}
ssh "$target" bash -s <<'REMOTE'
set -euo pipefail
cd /home/luke/inference/dspark_0731

printf 'captured_utc='; date -u +%FT%TZ
printf 'compose_sha256='; sha256sum docker-compose.yaml | awk '{print $1}'
printf 'driver='; nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -1
printf 'host_memory='; free -h | awk 'NR==2{print $2}'
printf 'numa_nodes='; lscpu | awk -F: '/NUMA node\(s\)/{gsub(/ /,"",$2); print $2}'
printf '%s\n' 'gpu_telemetry_begin'
nvidia-smi \
  --query-gpu=index,uuid,name,temperature.gpu,power.draw,power.limit,utilization.gpu,utilization.memory,memory.used,memory.total \
  --format=csv,noheader,nounits
printf '%s\n' 'gpu_telemetry_end'
printf '%s\n' 'gpu_thermal_power_limits_begin'
nvidia-smi -q -d TEMPERATURE,POWER
printf '%s\n' 'gpu_thermal_power_limits_end'

for container in dspark-0731 dspark-0731-b; do
  image=$(docker inspect -f '{{.Config.Image}}' "$container")
  image_id=$(docker inspect -f '{{.Image}}' "$container")
  init_pid=$(docker inspect -f '{{.State.Pid}}' "$container")
  cpus=$(docker inspect -f '{{.HostConfig.CpusetCpus}}' "$container")
  mems=$(docker inspect -f '{{.HostConfig.CpusetMems}}' "$container")
  printf '%s image=%s image_id=%s cpuset_cpus=%s cpuset_mems=%s\n' \
    "$container" "$image" "$image_id" "${cpus:-all}" "${mems:-all}"
  ps --ppid "$init_pid" -o args= \
    | sed -E 's/(--api-key(=| )[[:space:]]*)[^[:space:]]+/\1<redacted>/g' \
    | head -1
  docker logs "$container" 2>&1 \
    | grep -E 'GPU KV cache size|Maximum concurrency' \
    | tail -2
done

docker inspect -f '{{.Name}} image={{.Config.Image}} status={{.State.Status}}' \
  ds4-loadbalancer dspark-0731 dspark-0731-b
curl -fsS http://127.0.0.1:8007/metrics \
  | awk '/ramjet_upstream_(up|inflight|load_units)/ && $1 !~ /^#/ {print}'
nvidia-smi topo -m
REMOTE
