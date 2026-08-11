#!/usr/bin/env bash
# Fire N concurrent requests that SHARE a system prompt (same "app") but are
# distinct sessions. Measures the upstream split + aggregate throughput.
# Hash router: system prompt dominates the 4KB hash key -> all land on ONE
# instance (idle sibling). Overlap+load router: spreads by inflight.
set -uo pipefail
BASE=$1 N=$2 SALT=$3 TOK=$4
KEY=$(grep -o "Bearer [A-Za-z0-9_-]*" /etc/caddy/Caddyfile | head -1 | cut -d" " -f2)
SYS=$(python3 -c "print('You are coding agent for CONC-$SALT. ' + 'Follow the runbook carefully and cite file paths. ' * 1800)")
before_a=$(curl -s http://127.0.0.1:8007/metrics | awk -F'[ ]' '/upstream_requests_total.*code="200".*dspark-0731:8000/{print $2}')
before_b=$(curl -s http://127.0.0.1:8007/metrics | awk -F'[ ]' '/upstream_requests_total.*code="200".*dspark-0731-b:8000/{print $2}')
start=$(date +%s.%N)
for i in $(seq 1 $N); do
  curl -s -m 300 -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' "$BASE/v1/chat/completions" -d "$(python3 -c "
import json,sys
print(json.dumps({'model':'deepseek-v4-flash','messages':[{'role':'system','content':sys.argv[1]},{'role':'user','content':f'session $i: solve subtask $i briefly'}],'max_tokens':$TOK,'temperature':0}))
" "$SYS")" > /tmp/ca_$i.json &
done
wait
end=$(date +%s.%N)
after_a=$(curl -s http://127.0.0.1:8007/metrics | awk -F'[ ]' '/upstream_requests_total.*code="200".*dspark-0731:8000/{print $2}')
after_b=$(curl -s http://127.0.0.1:8007/metrics | awk -F'[ ]' '/upstream_requests_total.*code="200".*dspark-0731-b:8000/{print $2}')
python3 -c "
import json,glob,os
tot=0
for f in glob.glob('/tmp/ca_*.json'):
    try: tot+=json.load(open(f)).get('usage',{}).get('completion_tokens',0)
    except: pass
    os.remove(f)
w=$end-$start
da=${after_a:-0}-${before_a:-0}; db=${after_b:-0}-${before_b:-0}
print(f'  split A/B = {da}/{db}  wall={w:.1f}s  aggregate={tot/w:.0f} tok/s')
"
