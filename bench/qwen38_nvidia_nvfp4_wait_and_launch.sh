#!/usr/bin/env bash
# Wait for node06's intake-air admission point, then run the isolated canary.
set -Eeuo pipefail

deployment=/home/luke/inference/qwen38_flash_next
experiment=$deployment/.experiments/20260907T1559Z-nvidia-nvfp4-tp4
canary=$experiment/qwen38_nvidia_nvfp4_canary.sh
canary_sha=3ff38556a0b0ca4813e15989c1c49c265991f0fc5dcf1e88942f163008b26195
lock_file=/run/lock/ramjet-node06-deployment.lock
state_file=$experiment/cooldown-wait.txt
deadline=$((SECONDS + 21600))

fail() {
  echo "NVIDIA NVFP4 cooldown owner: $*" >&2
  exit 2
}

intake_c() {
  curl -fsS --max-time 5 http://127.0.0.1:9100/metrics |
    awk '/^node_ipmi_temperature_celsius[{]/ && /sensor="FP_TEMP"/ {print $NF}'
}

[[ $(hostname) == node06 ]] || fail "this owner may run only on node06"
[[ $(id -u) == 0 ]] || fail "this owner must run as root"
[[ $(sha256sum "$canary" | awk '{print $1}') == "$canary_sha" ]] ||
  fail "canary authority changed"
[[ ! -e $state_file ]] || fail "cooldown output already exists"

umask 077
printf '%s result=waiting\n' "$(date -u +%FT%TZ)" >"$state_file"
next_checkpoint=$SECONDS

while ((SECONDS < deadline)); do
  temperature=$(intake_c)
  [[ $temperature =~ ^[0-9]+([.][0-9]+)?$ ]] || fail "intake telemetry is invalid"

  if ((SECONDS >= next_checkpoint)); then
    printf '%s intake_c=%s\n' "$(date -u +%FT%TZ)" "$temperature" >>"$state_file"
    next_checkpoint=$((SECONDS + 300))
  fi

  if awk -v value="$temperature" 'BEGIN {exit !(value <= 46)}'; then
    if flock -n "$lock_file" true; then
      printf '%s result=admitted intake_c=%s\n' "$(date -u +%FT%TZ)" "$temperature" >>"$state_file"
      exec /bin/bash "$canary"
    fi
  fi
  sleep 30
done

fail "the six-hour cooling deadline expired"
