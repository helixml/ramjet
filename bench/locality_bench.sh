#!/usr/bin/env bash
# Cache-locality benchmark: APPS simulated apps (shared big system prompt) x
# SESSIONS sessions x TURNS turns. Reports cached-token %, mean TTFB-ish
# request wall time per turn, and the upstream split. Compare a static-hash
# LB vs ramjet's overlap router with identical traffic.
#   BENCH_TOKEN=<bearer> ./locality_bench.sh [base] [apps] [sessions] [turns]
# For engines whose response cached_tokens is not authoritative:
#   CACHE_AUTHORITY=vllm-prefix ENGINE_METRICS_URLS=url-a,url-b ...
set -euo pipefail
BASE="${1:-http://127.0.0.1:8006}"
APPS="${2:-2}"; SESSIONS="${3:-4}"; TURNS="${4:-3}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CACHE_AUTHORITY="${CACHE_AUTHORITY:-response}"
ENGINE_METRICS_URLS="${ENGINE_METRICS_URLS:-}"

case "$CACHE_AUTHORITY" in
  response) ;;
  vllm-prefix)
    if [[ -z "$ENGINE_METRICS_URLS" ]]; then
      echo "CACHE_AUTHORITY=vllm-prefix requires ENGINE_METRICS_URLS" >&2
      exit 2
    fi
    ;;
  *)
    echo "CACHE_AUTHORITY must be response or vllm-prefix" >&2
    exit 2
    ;;
esac

snapshot_engine_metrics() {
  PYTHONPATH="$SCRIPT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -c '
import json, sys
from engine_metrics import fetch
urls = [item.strip() for item in sys.argv[1].split(",") if item.strip()]
print(json.dumps([fetch(url) for url in urls], sort_keys=True))
' "$ENGINE_METRICS_URLS"
}

if [[ "$CACHE_AUTHORITY" == vllm-prefix ]]; then
  snapshot_engine_metrics > "$TMP/engine-before.json"
fi

sys_prompt() { # ~20KB deterministic per-app system prompt
  python3 -c "print('You are coding agent for APP-${SALT:-x}-$1. ' + ('Follow the runbook carefully and cite file paths. ' * 1) * 1800 + 'APPSALT${SALT:-x}TAG$1' * 50)"
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
  curl -s -m 300 -H "Authorization: Bearer ${BENCH_TOKEN:-none}" -H 'Content-Type: application/json' "$BASE/v1/chat/completions" -d "{
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
if [[ "$CACHE_AUTHORITY" == vllm-prefix ]]; then
  snapshot_engine_metrics > "$TMP/engine-after.json"
  native_fields=$(PYTHONPATH="$SCRIPT_DIR${PYTHONPATH:+:$PYTHONPATH}" python3 -c '
import json, sys
from engine_metrics import aggregate_deltas, cache_usage
with open(sys.argv[1], encoding="utf-8") as source:
    before = json.load(source)
with open(sys.argv[2], encoding="utf-8") as source:
    after = json.load(source)
usage = cache_usage(int(sys.argv[3]), int(sys.argv[4]), aggregate_deltas(before, after), "vllm-prefix")
if not usage["available"]:
    raise SystemExit("native vLLM prefix counters are unavailable or invalid")
print("{:g} {:g} {:.1f}".format(
    usage["prompt_tokens"], usage["cached_tokens"], usage["hit_pct"]
))
' "$TMP/engine-before.json" "$TMP/engine-after.json" "$total_p" "$total_c")
  read -r native_p native_c native_hit <<< "$native_fields"
  echo "TOTAL prompt=$native_p cached=$native_c hit=${native_hit}% authority=vllm_prefix_counters response_prompt=$total_p response_cached=$total_c"
else
  echo "TOTAL prompt=$total_p cached=$total_c hit=$(python3 -c "print(f'{$total_c/max($total_p,1)*100:.1f}%')")"
fi
