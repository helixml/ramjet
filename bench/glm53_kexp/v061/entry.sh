#!/bin/bash
# v0.6.1 candidate: optionally apply the TP2 chunked PCIe IPC patch, then serve.
set -euo pipefail
if [ "${KEXP_APPLY_CHUNK_PATCH:-0}" = "1" ]; then
  python3 /opt/kexp/patch-pcie-ipc-chunked-v061.py
fi
exec sglang "$@"
