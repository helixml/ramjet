#!/usr/bin/env bash
# Cache-locality benchmark: APPS simulated apps (shared big system prompt) x
# SESSIONS sessions x TURNS turns. Reports cached-token %, mean TTFB-ish
# request wall time per turn, and the upstream split. Compare a static-hash
# LB vs mini-dynamo's overlap router with identical traffic.
#   BENCH_TOKEN=<bearer> ./locality_bench.sh [base] [apps] [sessions] [turns]
set -euo pipefail
BASE="${1:-http://127.0.0.1:8006}"
APPS="${2:-2}"; SESSIONS="${3:-4}"; TURNS="${4:-3}"
AUTH=${BENCH_TOKEN:+-H "Authorization: Bearer $BENCH_TOKEN"}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

sys_prompt() { # ~20KB deterministic per-app system prompt
  python3 -c "import sys; print(('You are agent for APP-$1. ' + 'Follow the runbook carefully. ' * 40 + chr(65+$1) * 100)[:20000])"
}

turn() { # app session turn -> one request, print "cached prompt wall"
  local app=$1 sess=$2 turn=$3
  local sys; sys=$(sys_prompt "$app")
  local history=""
  for ((h=1; h<turn; h++)); do
    history+=", {\"role\":\"user\",\"content\":\"session $sess step $h: describe part $h\"}, {\"role\":\"assistant\",\"content\":\"(answer $h for session $sess)\"}"
  done
  local start end
  start=$(date +%s.%N)
  curl -s -m 300 $AUTH -H 'Content-Type: application/json' "$BASE/v1/chat/completions" -d "{
    \"model\": \"deepseek-v4-flash\",
    \"messages\": [{\"role\":\"system\",\"content\": $(python3 -c "import json,sys; print(json.dumps(sys.argv[1]))" "$sys")}$history,
      {\"role\":\"user\",\"content\":\"session $sess step $turn: describe part $turn briefly\"}],
    \"max_tokens\": 60, \"temperature\": 0
  }" > "$TMP/r.json"
  end=$(date +%s.%N)
  python3 -c "
import json,sys
r=json.load(open('$TMP/r.json')); u=r.get('usage',{})
print(u.get('prompt_tokens',0), (u.get('prompt_tokens_details') or {}).get('cached_tokens',0), round($end-$start,2))"
}

echo "app session turn prompt cached wall_s"
total_p=0; total_c=0
for ((t=1; t<=TURNS; t++)); do
  for ((a=0; a<APPS; a++)); do
    for ((s=0; s<SESSIONS; s++)); do
      read -r p c w <<< "$(turn "$a" "$s" "$t")"
      echo "$a $s $t $p $c $w"
      total_p=$((total_p+p)); total_c=$((total_c+c))
    done
  done
done
echo "TOTAL prompt=$total_p cached=$total_c hit=$(python3 -c "print(f'{$total_c/max($total_p,1)*100:.1f}%')")"
