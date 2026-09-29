#!/usr/bin/env bash
# One sglang.bench_serving cell against an OpenAI chat endpoint.
#   serving-cell.sh LABEL BASE_URL CONCURRENCY NUM_PROMPTS INPUT_LEN OUTPUT_LEN [extra bench_serving args]
# Appends the bench_serving JSON record to $RESULTS/serving.jsonl and prints a
# one-line summary. Random prompts use a fresh seed per cell so no cell is
# served from a previous cell's prefix cache.
set -euo pipefail
label=$1 base=$2 conc=$3 num=$4 in_len=$5 out_len=$6
shift 6
results=${RESULTS:-${GLM_H200_ROOT:-$HOME}/results}
model_dir=${MODEL_DIR:-${GLM_H200_ROOT:-$HOME}/models/zai-org/GLM-5.3-Flash-eb9eb208}
image=${SGLANG_IMAGE:-lmsysorg/sglang@sha256:06e4f2ed21afde4ff513cda65070124e727ba23ccaeff7712b8c40e1097d611f}
seed=${SEED:-$(date +%s)}
mkdir -p "$results"
docker run --rm --network host \
  -v "$model_dir":/models/glm53:ro -v "$results":/results \
  --entrypoint python3 "$image" -m sglang.bench_serving \
  --backend sglang-oai-chat --base-url "$base" --model glm-5.3-flash \
  --tokenizer /models/glm53 --dataset-name random \
  --random-input-len "$in_len" --random-output-len "$out_len" --random-range-ratio 1.0 \
  --num-prompts "$num" --max-concurrency "$conc" --request-rate inf \
  --seed "$seed" --tag "$label" --output-file /results/serving.jsonl "$@" \
  >"$results/serving-$label.log" 2>&1
python3 - "$results/serving.jsonl" "$label" <<'EOF'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
r = [x for x in rows if x.get("tag") == sys.argv[2]][-1]
print(json.dumps({
    "label": sys.argv[2], "conc": r.get("max_concurrency"), "ok": r.get("completed"),
    "dur_s": round(r["duration"], 1), "in_tok_s": round(r["input_throughput"]),
    "out_tok_s": round(r["output_throughput"]),
    "ttft_ms_p50": round(r["median_ttft_ms"]), "ttft_ms_p99": round(r["p99_ttft_ms"]),
    "tpot_ms_p50": round(r["median_tpot_ms"], 1), "itl_ms_p50": round(r["median_itl_ms"], 1),
}))
EOF
