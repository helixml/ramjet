#!/usr/bin/env bash
# prewarm.sh [MODEL_DIR] [STREAMS] — pull the checkpoint into the page cache with
# parallel sequential reads before `docker compose up`. The last two shards hold
# the Engram tables, which the replicas copy straight into shared memory, so
# warming them as well only adds memory pressure.
set -euo pipefail
dir=${1:-${MODEL_DIR:-${DS_H200_ROOT:-$HOME}/models/deepseek-ai/DeepSeek-V4.1-Flash-2cba9e42}}
streams=${2:-32}
start=$(date +%s)
find "$dir" -name '*.safetensors' ! -name 'model-0004[78]-of-00048.safetensors' -print0 |
  xargs -0 -P "$streams" -I{} dd if={} of=/dev/null bs=16M status=none
echo "prewarmed $dir with $streams streams in $(( $(date +%s) - start ))s"
