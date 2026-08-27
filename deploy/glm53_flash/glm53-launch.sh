#!/usr/bin/env bash
set -euo pipefail

case ${GLM_MTP_MODE:-off} in
  off|on) ;;
  *) echo "GLM_MTP_MODE must be off or on" >&2; exit 2 ;;
esac

case ${GLM_MTP_ADAPTIVE:-on} in
  off|on) ;;
  *) echo "GLM_MTP_ADAPTIVE must be off or on" >&2; exit 2 ;;
esac

args=(
  -m sglang.launch_server
  --model-path /workspace/model
  --served-model-name glm-5.3-flash
  --host 0.0.0.0
  --port 8000
  --tp-size 4
  --ep-size 4
  --context-length "${GLM_CONTEXT_LENGTH:-262144}"
  --quantization modelopt_fp4
  --attention-backend dsa
  --dsa-prefill-backend flashinfer_sparse_mla
  --dsa-decode-backend flashinfer_sparse_mla
  --linear-attn-backend triton
  --kv-cache-dtype fp8_e4m3
  --moe-runner-backend flashinfer_cutlass
  --disable-shared-experts-fusion
  --chunked-prefill-size 8192
  --max-prefill-tokens 8192
  --max-running-requests "${GLM_MAX_RUNNING_REQUESTS:-4}"
  --mem-fraction-static "${GLM_MEM_FRACTION_STATIC:-0.90}"
  --cuda-graph-max-bs-decode "${GLM_CUDA_GRAPH_MAX_BS:-4}"
  --enable-metrics
  --enable-cache-report
  --media-url-max-file-size-mb 1024
  --enable-multimodal
  --chat-template /chat-template.jinja
  --reasoning-parser glm45
  --tool-call-parser glm47
)

if [[ -n ${GLM_MAX_MAMBA_CACHE_SIZE:-} ]]; then
  args+=(--max-mamba-cache-size "${GLM_MAX_MAMBA_CACHE_SIZE}")
fi

if [[ ${GLM_MTP_MODE:-off} == on ]]; then
  args+=(
    --speculative-algorithm EAGLE
    --speculative-num-steps "${GLM_MTP_STEPS:-5}"
    --speculative-eagle-topk 1
    --speculative-num-draft-tokens "${GLM_MTP_DRAFT_TOKENS:-6}"
  )
  if [[ ${GLM_MTP_ADAPTIVE:-on} == on ]]; then
    args+=(--speculative-adaptive)
  fi
fi

exec python3 "${args[@]}"
