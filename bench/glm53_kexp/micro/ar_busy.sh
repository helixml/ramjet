#!/bin/bash
# All-reduce microbench with N busy-spinning processes sharing the container's cpuset.
n=${BUSY:-0}
for i in $(seq 1 $n); do python3 -c "while True: pass" & done
torchrun --nproc_per_node 2 /ar.py 2>&1 | grep -E "tokens= (1024|6144)"
kill $(jobs -p) 2>/dev/null
