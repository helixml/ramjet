#!/usr/bin/env bash
# Routing A/B on the 2x TP4 fleet: identical swarm structure, fresh text per run.
#   CONFIGS="absolute marginal load" RUN_SUFFIX=-r1 ab-sequence.sh
# absolute and load run the v0.6.2 control image; marginal runs main after #294 and #296.
set -euo pipefail
D=$(cd "$(dirname "$0")" && pwd)
cd "$D"
V062=ghcr.io/helixml/ramjet:v0.6.2@sha256:53047a816c8ae1dbade7e27b86d2e3eda40cfeaa48826ae06bf2ac1d35a27cc6
MAIN=ghcr.io/helixml/ramjet:rust-a524263@sha256:7f874182ee28dca1764454107647fcba67696292481d8c068b5ca9ab8ce3092c
if [[ "${SKIP_WARM:-0}" != 1 ]]; then
  for port in 8070 8071; do
    ./serving-cell.sh "warm-$port-$(date +%s)" "http://127.0.0.1:$port" 48 96 16000 256 --random-range-ratio 0.05 >/dev/null &
  done
  wait
fi
run() {
  local label=$1 image=$2; shift 2
  env LB_IMAGE="$image" "$@" docker compose up -d --force-recreate ramjet >/dev/null 2>&1
  sleep 8
  up=$(curl -s localhost:8007/metrics | grep -c '^ramjet_upstream_up{.*} 1')
  [[ "$up" == 2 ]] || { echo "ramjet has $up/2 upstreams up" >&2; exit 1; }
  echo "== $label $(docker inspect ramjet --format '{{.Config.Image}}') $(docker inspect ramjet --format '{{range .Config.Env}}{{println .}}{{end}}' | grep -E '^RJ_(AFFINITY|ROUTE_AFFINITY_BASIS)=' | tr '\n' ' ')"
  bash "$D/swarm-cell.sh" "$label" http://127.0.0.1:8006 glm53-a glm53-b
}
for cfg in ${CONFIGS:-absolute marginal load}; do
  case $cfg in
    absolute) run rj2x4-prefix-absolute${RUN_SUFFIX:-} "$V062" RJ_AFFINITY=prefix RJ_ROUTE_AFFINITY_BASIS=absolute ;;
    marginal) run rj2x4-prefix-marginal${RUN_SUFFIX:-} "$MAIN" RJ_AFFINITY=prefix RJ_ROUTE_AFFINITY_BASIS=marginal ;;
    load) run rj2x4-load${RUN_SUFFIX:-} "$V062" RJ_AFFINITY=load RJ_ROUTE_AFFINITY_BASIS=absolute ;;
  esac
done
