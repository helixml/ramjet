#!/usr/bin/env bash
# Capture a bounded, privacy-safe node06 serving snapshot.
#
# Backward-compatible default:
#   bash bench/capture_node06.sh node06
#
# Qwen3.8-Flash-Next direct candidate:
#   bash bench/capture_node06.sh --profile qwen38-flash-next node06
#
# Explicit deployment/container selection:
#   bash bench/capture_node06.sh \
#     --deployment-dir /home/luke/inference/qwen38_flash_next \
#     --engine qwen38flashnext-b --direct-candidate qwen38flashnext-b node06
set -euo pipefail

usage() {
  cat <<'EOF'
usage: capture_node06.sh [OPTIONS] [ssh-host]

Options:
  --profile NAME          dspark-0731 (default) or qwen38-flash-next
  --deployment-dir PATH  absolute remote deployment directory
  --compose-file NAME    Compose filename inside the deployment directory
  --engine CONTAINER     engine to capture; repeat for multiple engines
  --direct-candidate C   identify one captured engine as the direct candidate
  --lb-container C       load-balancer container (default: ds4-loadbalancer)
  --lb-metrics-url URL   loopback Prometheus endpoint (default: :8007/metrics)
  --local                capture the local host instead of using SSH
  -h, --help             show this help
EOF
}

fail() {
  echo "capture_node06.sh: $*" >&2
  exit 2
}

valid_container() {
  [[ $1 =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]]
}

one_line() {
  # Docker labels and image references are external strings. Keep every
  # selected field printable, single-line, and bounded before emitting it.
  LC_ALL=C awk '
    {
      gsub(/[[:cntrl:]]/, " ")
      text = text (text == "" ? "" : " ") $0
    }
    END { print substr(text, 1, 512) }
  '
}

route_status() {
  local engine=$1 upstreams=$2 endpoint authority host
  [[ -n $upstreams ]] || { printf 'unknown\n'; return; }
  IFS=, read -r -a endpoints <<<"$upstreams"
  for endpoint in "${endpoints[@]}"; do
    endpoint=${endpoint#*://}
    authority=${endpoint%%/*}
    host=${authority%%:*}
    if [[ $host == "$engine" ]]; then
      printf 'routed\n'
      return
    fi
  done
  printf 'unrouted\n'
}

remote_capture() {
  local deployment_dir=$1 compose_file=$2 lb_container=$3
  local lb_metrics_url=$4 direct_candidate=$5
  shift 5
  local -a engines=("$@")
  local compose_path

  [[ $deployment_dir == /* ]] || fail "deployment directory must be absolute"
  [[ $compose_file =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || \
    fail "compose file must be a filename inside the deployment directory"
  valid_container "$lb_container" || fail "invalid load-balancer container"
  [[ $lb_metrics_url =~ ^http://127\.0\.0\.1:[0-9]+/[A-Za-z0-9_./-]*$ ]] || \
    fail "load-balancer metrics URL must use numeric loopback"
  ((${#engines[@]} > 0)) || fail "at least one engine is required"
  local engine
  for engine in "${engines[@]}"; do
    valid_container "$engine" || fail "invalid engine container: $engine"
  done
  if [[ -n $direct_candidate ]]; then
    valid_container "$direct_candidate" || fail "invalid direct candidate"
    local found=0
    for engine in "${engines[@]}"; do
      [[ $engine == "$direct_candidate" ]] && found=1
    done
    ((found == 1)) || fail "direct candidate must also be a captured engine"
  fi

  compose_path=$deployment_dir/$compose_file
  [[ -r $compose_path ]] || fail "unreadable Compose file: $compose_path"

  printf 'captured_utc='; date -u +%FT%TZ
  printf 'deployment_dir=%q\n' "$deployment_dir"
  printf 'compose_file=%q\n' "$compose_file"
  printf 'compose_sha256='; sha256sum "$compose_path" | awk '{print $1}'
  printf 'driver='; \
    nvidia-smi --query-gpu=driver_version --format=csv,noheader | \
      awk 'NR == 1 {value=$0} END {print value}'
  printf 'host_memory_total='; free -h | awk 'NR==2{print $2}'
  printf 'host_memory_available='; \
    awk '/^MemAvailable:/{printf "%.1fGiB\n", $2/1048576}' /proc/meminfo
  printf 'host_swap_used='; \
    free -b | awk 'NR==3{printf "%.1fGiB of %.1fGiB\n", ($2-$4)/1073741824, $2/1073741824}'
  printf 'zfs_arc_size='; \
    awk '/^size/{printf "%.2fGiB", $3/1073741824}' /proc/spl/kstat/zfs/arcstats \
      2>/dev/null || printf 'none'
  awk '/^c_max/{printf " (cap %.2fGiB)", $3/1073741824}' \
    /proc/spl/kstat/zfs/arcstats 2>/dev/null || true
  echo
  printf 'numa_free='; \
    numactl --hardware 2>/dev/null | awk '/free:/{printf "node%s=%sMB ", ++n-1, $4}'
  echo
  printf 'numa_nodes='; \
    lscpu | awk -F: '/NUMA node\(s\)/{gsub(/ /,"",$2); print $2}'

  local air_body intake
  air_body=$(curl -fsS --max-time 5 http://127.0.0.1:9100/metrics)
  intake=$(awk '
    /node_ipmi_temperature_celsius\{/ {
      if ($0 ~ /sensor="FP_TEMP"/) fp = $NF
      if ($0 ~ /sensor="Inlet Temp"/) inlet = $NF
    }
    END {
      if (fp != "") print "FP_TEMP " fp
      else if (inlet != "") print "Inlet_Temp " inlet
    }
  ' <<<"$air_body")
  [[ -n $intake ]] || fail "node exporter exposed no FP_TEMP/Inlet Temp reading"
  read -r intake_sensor intake_c <<<"$intake"
  [[ $intake_c =~ ^-?[0-9]+([.][0-9]+)?$ ]] || fail "invalid intake reading"
  printf 'intake_sensor=%s intake_air_c=%s\n' "$intake_sensor" "$intake_c"

  printf '%s\n' 'gpu_telemetry_begin'
  nvidia-smi \
    --query-gpu=index,name,temperature.gpu,power.draw,power.limit,utilization.gpu,utilization.memory,memory.used,memory.total \
    --format=csv,noheader,nounits
  printf '%s\n' 'gpu_telemetry_end'
  printf '%s\n' 'gpu_thermal_power_limits_begin'
  nvidia-smi -q -d TEMPERATURE,POWER | awk '
    /^GPU [0-9a-fA-F:.]+/ ||
    /Slowdown Temp/ || /Shutdown Temp/ ||
    /Current Power Limit/ || /Default Power Limit/ { print substr($0, 1, 256) }
  '
  printf '%s\n' 'gpu_thermal_power_limits_end'

  local lb_status lb_image lb_image_id upstreams metrics_body
  docker inspect "$lb_container" >/dev/null
  lb_status=$(docker inspect --format '{{.State.Status}}' "$lb_container" | one_line)
  lb_image=$(docker inspect --format '{{.Config.Image}}' "$lb_container" | one_line)
  lb_image_id=$(docker inspect --format '{{.Image}}' "$lb_container" | one_line)
  upstreams=$(
    docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$lb_container" |
      awk -F= '$1 == "RJ_UPSTREAM" {sub(/^[^=]*=/, ""); value=$0} END {print value}'
  )
  printf 'load_balancer=%q status=%q image=%q image_id=%q\n' \
    "$lb_container" "$lb_status" "$lb_image" "$lb_image_id"

  metrics_body=$(curl -fsS --max-time 5 "$lb_metrics_url")
  awk '
    $1 ~ /^ramjet_upstream_up([{]|$)/ {up_count++; up_sum += $NF}
    $1 ~ /^ramjet_upstream_inflight([{]|$)/ {inflight_count++; inflight_sum += $NF}
    $1 ~ /^ramjet_upstream_load_units([{]|$)/ {load_count++; load_sum += $NF}
    END {
      printf "lb_metrics_up_series=%d lb_metrics_up_sum=%.0f ", up_count, up_sum
      printf "lb_metrics_inflight_sum=%.0f lb_metrics_load_units_sum=%.0f\n", inflight_sum, load_sum
    }
  ' <<<"$metrics_body"

  local configured_image image_id status started restarts cpus mems
  local model_repository model_revision argv_sha256 routing role
  for engine in "${engines[@]}"; do
    configured_image=$(docker inspect --format '{{.Config.Image}}' "$engine" | one_line)
    image_id=$(docker inspect --format '{{.Image}}' "$engine" | one_line)
    status=$(docker inspect --format '{{.State.Status}}' "$engine" | one_line)
    started=$(docker inspect --format '{{.State.StartedAt}}' "$engine" | one_line)
    restarts=$(docker inspect --format '{{.RestartCount}}' "$engine" | one_line)
    cpus=$(docker inspect --format '{{.HostConfig.CpusetCpus}}' "$engine" | one_line)
    mems=$(docker inspect --format '{{.HostConfig.CpusetMems}}' "$engine" | one_line)
    model_repository=$(docker inspect --format \
      '{{with .Config.Labels}}{{index . "ai.ramjet.model.repository"}}{{end}}' \
      "$engine" | one_line)
    model_revision=$(docker inspect --format \
      '{{with .Config.Labels}}{{index . "ai.ramjet.model.revision"}}{{end}}' \
      "$engine" | one_line)
    # Never print process argv or container environment. Hash Docker's exact
    # executable/argument JSON so snapshots still detect launcher drift even
    # when an older deployment put bearer material in argv.
    argv_sha256=$(
      docker inspect --format '{{json .Path}}{{json .Args}}' "$engine" |
        sha256sum | awk '{print $1}'
    )
    routing=$(route_status "$engine" "$upstreams")
    role=engine
    if [[ $engine == "$direct_candidate" ]]; then
      role=direct_candidate
      [[ $routing != unrouted ]] || routing=direct-only
    fi
    printf 'engine=%q role=%s route_status=%s status=%q restart_count=%q started_at=%q image=%q image_id=%q cpuset_cpus=%q cpuset_mems=%q model_repository=%q model_revision=%q argv_sha256=%s\n' \
      "$engine" "$role" "$routing" "$status" "$restarts" "$started" \
      "$configured_image" "$image_id" "$cpus" "$mems" \
      "$model_repository" "$model_revision" "$argv_sha256"
    # Extract reviewed numeric capacity fields from a bounded log tail. Never
    # replay even a matching raw line: launcher wrappers can append argv or
    # other operational text to it.
    docker logs --tail 200 "$engine" 2>&1 | awk '
      /GPU KV cache size/ {
        value=$0
        sub(/^.*GPU KV cache size[^0-9]*/, "", value)
        sub(/[^0-9,].*$/, "", value)
        gsub(/,/, "", value)
        if (value ~ /^[0-9]+$/) kv=value
      }
      /Maximum concurrency for/ {
        context=$0
        sub(/^.*Maximum concurrency for[[:space:]]*/, "", context)
        sub(/[^0-9,].*$/, "", context)
        gsub(/,/, "", context)
        multiplier=$0
        sub(/^.*per request:[[:space:]]*/, "", multiplier)
        sub(/[^0-9.].*$/, "", multiplier)
        if (context !~ /^[0-9]+$/) context=""
        if (multiplier !~ /^[0-9]+([.][0-9]+)?$/) multiplier=""
      }
      END {
        if (kv != "") print "engine_capacity_kv_tokens=" kv
        if (context != "" && multiplier != "")
          print "engine_capacity_context_tokens=" context " multiplier=" multiplier
      }
    '
  done

  docker inspect --format '{{.Name}} status={{.State.Status}}' \
    "$lb_container" "${engines[@]}" | one_line
  nvidia-smi topo -m
}

if [[ ${1:-} == --remote ]]; then
  shift
  remote_capture "$@"
  exit
fi

profile=dspark-0731
deployment_dir=
compose_file=docker-compose.yaml
lb_container=ds4-loadbalancer
lb_metrics_url=http://127.0.0.1:8007/metrics
direct_candidate=
local_capture=0
declare -a requested_engines=()
declare -a positional=()

while (($#)); do
  case $1 in
    --profile|--deployment-dir|--compose-file|--engine|--direct-candidate|--lb-container|--lb-metrics-url)
      (($# >= 2)) || fail "missing value for $1"
      option=$1
      value=$2
      shift 2
      case $option in
        --profile) profile=$value ;;
        --deployment-dir) deployment_dir=$value ;;
        --compose-file) compose_file=$value ;;
        --engine) requested_engines+=("$value") ;;
        --direct-candidate) direct_candidate=$value ;;
        --lb-container) lb_container=$value ;;
        --lb-metrics-url) lb_metrics_url=$value ;;
      esac
      ;;
    --local)
      local_capture=1
      shift
      ;;
    -h|--help)
      usage
      exit
      ;;
    --*) fail "unknown option: $1" ;;
    *) positional+=("$1"); shift ;;
  esac
done

((${#positional[@]} <= 1)) || fail "at most one ssh host may be supplied"
target=${positional[0]:-node06}
[[ $target =~ ^[A-Za-z0-9_.@-]+$ ]] || fail "invalid ssh host"

declare -a default_engines
case $profile in
  dspark-0731)
    deployment_dir=${deployment_dir:-/home/luke/inference/dspark_0731}
    default_engines=(dspark-0731 dspark-0731-b)
    ;;
  qwen38-flash-next)
    deployment_dir=${deployment_dir:-/home/luke/inference/qwen38_flash_next}
    default_engines=(qwen38flashnext-b)
    ;;
  *) fail "unknown profile: $profile" ;;
esac

declare -a engines
if ((${#requested_engines[@]})); then
  engines=("${requested_engines[@]}")
else
  engines=("${default_engines[@]}")
  if [[ $profile == qwen38-flash-next && -z $direct_candidate ]]; then
    direct_candidate=qwen38flashnext-b
  fi
fi

if ((local_capture)); then
  remote_capture "$deployment_dir" "$compose_file" "$lb_container" \
    "$lb_metrics_url" "$direct_candidate" "${engines[@]}"
else
  ssh "$target" bash -s -- --remote "$deployment_dir" "$compose_file" \
    "$lb_container" "$lb_metrics_url" "$direct_candidate" "${engines[@]}" <"$0"
fi
