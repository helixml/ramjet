#!/usr/bin/env bash
# Fire N concurrent requests that SHARE a system prompt (same "app") but are
# distinct sessions. Measures the upstream split + aggregate throughput.
# Hash router: system prompt dominates the 4KB hash key -> all land on ONE
# instance (idle sibling). Overlap+load router: spreads by inflight.
set -uo pipefail
BASE=$1 N=$2 SALT=$3 TOK=$4
MODEL=${BENCH_MODEL:-deepseek-v4-flash}
KEY=${BENCH_TOKEN:-${VLLM_API_KEY:-}}
if [ -z "$KEY" ]; then
  KEY=$(grep -o "Bearer [A-Za-z0-9_-]*" /etc/caddy/Caddyfile | head -1 | cut -d" " -f2)
fi
SYS=$(python3 -c "print('You are coding agent for CONC-$SALT. ' + 'Follow the runbook carefully and cite file paths. ' * 1800)")
WORK=$(mktemp -d /tmp/ramjet-sameapp.XXXXXX)
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT
start=$(date +%s.%N)
pids=()
for i in $(seq 1 $N); do
  curl -sS -m 300 -D "$WORK/$i.headers" -o "$WORK/$i.json" \
    -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' "$BASE/v1/chat/completions" -d "$(python3 -c "
import json,sys
print(json.dumps({'model':sys.argv[2],'messages':[{'role':'system','content':sys.argv[1]},{'role':'user','content':f'session $i: solve subtask $i briefly'}],'max_tokens':$TOK,'temperature':0}))
" "$SYS" "$MODEL")" &
  pids+=("$!")
done
curl_failures=0
for pid in "${pids[@]}"; do
  wait "$pid" || curl_failures=$((curl_failures + 1))
done
end=$(date +%s.%N)
WORK="$WORK" START="$start" END="$end" N="$N" CURL_FAILURES="$curl_failures" python3 - <<'PY'
import glob
import json
import os

work = os.environ["WORK"]
tokens = 0
errors = int(os.environ["CURL_FAILURES"])
routes = {"0": 0, "1": 0}
for path in glob.glob(work + "/*.json"):
    try:
        with open(path) as source:
            response = json.load(source)
        tokens += response.get("usage", {}).get("completion_tokens", 0)
        if response.get("error"):
            errors += 1
    except Exception:
        errors += 1
for path in glob.glob(work + "/*.headers"):
    with open(path, errors="replace") as source:
        for line in source:
            if line.lower().startswith("x-ramjet-upstream:"):
                route = line.split(":", 1)[1].strip()
                routes[route] = routes.get(route, 0) + 1
wall = float(os.environ["END"]) - float(os.environ["START"])
routed = sum(routes.values())
print(
    f"  split A/B = {routes.get('0', 0)}/{routes.get('1', 0)} "
    f"routed={routed}/{os.environ['N']} failures={errors} "
    f"wall={wall:.1f}s aggregate={tokens / wall:.0f} tok/s"
)
raise SystemExit(0 if routed == int(os.environ["N"]) and errors == 0 else 1)
PY
