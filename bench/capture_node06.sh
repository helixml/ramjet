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
printf 'host_memory_total='; free -h | awk 'NR==2{print $2}'
# Total alone hides the constraint that actually binds. node06's RAM is
# dominated by ZFS ARC, which the kernel reports as used and excludes from
# MemAvailable, so it is invisible to both `free -h` triage and `docker stats`.
# Capturing only the total is why "~8-9GiB available" went unattributed across
# four months of experiment entries.
printf 'host_memory_available='; awk '/^MemAvailable:/{printf "%.1fGiB\n", $2/1048576}' /proc/meminfo
printf 'host_swap_used='; free -b | awk 'NR==3{printf "%.1fGiB of %.1fGiB\n", ($2-$4)/1073741824, $2/1073741824}'
printf 'zfs_arc_size='; awk '/^size/{printf "%.2fGiB", $3/1073741824}' /proc/spl/kstat/zfs/arcstats 2>/dev/null || printf 'none'
awk '/^c_max/{printf " (cap %.2fGiB)", $3/1073741824}' /proc/spl/kstat/zfs/arcstats 2>/dev/null; echo
printf 'numa_free='; numactl --hardware 2>/dev/null | awk '/free:/{printf "node%s=%sMB ", ++n-1, $4}'; echo
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
