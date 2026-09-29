#!/usr/bin/env bash
# Parallel direct comparison: replica A (candidate) vs replica B (control), identical prompts per cell.
set -euo pipefail
cd "$(dirname "$0")"
ta=${TAG_A:-mtp-a} tb=${TAG_B:-base-b}
for cell in "c1 1 8" "c16 16 64" "c64 64 192"; do
  read -r name conc num <<<"$cell"
  s=$RANDOM
  SEED=$s ./serving-cell.sh "$ta-8k1k-$name" http://127.0.0.1:8070 "$conc" "$num" 8192 1024 &
  SEED=$s ./serving-cell.sh "$tb-8k1k-$name" http://127.0.0.1:8071 "$conc" "$num" 8192 1024 &
  wait
done
