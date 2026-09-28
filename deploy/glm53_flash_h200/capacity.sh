#!/usr/bin/env bash
# Capacity curve through ramjet on the current canonical fleet.
set -euo pipefail
D=$(cd "$(dirname "$0")" && pwd)
until curl -sf -m5 localhost:8070/health >/dev/null && curl -sf -m5 localhost:8071/health >/dev/null; do sleep 10; done
(cd $D && ./serving-cell.sh "capwarm-a-$(date +%s)" http://127.0.0.1:8070 48 96 16000 256 --random-range-ratio 0.05 >/dev/null)
sleep 10
for n in ${SIZES:-96 128}; do
  echo "== developers=$n LB=$(docker inspect ramjet --format '{{.Config.Image}}')"
  DEVELOPERS=$n bash $D/swarm-cell.sh "cap-marginal-mtp-$n" http://127.0.0.1:8006 glm53-a glm53-b
done
