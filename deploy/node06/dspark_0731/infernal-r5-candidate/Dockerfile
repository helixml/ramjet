# syntax=docker/dockerfile:1.7

# The immutable r4 runtime already contains the compiled CUDA/PyTorch/vLLM
# extension set. This overlay changes Python parser and attention-metadata
# sources only; it deliberately does not rebuild native extensions.
ARG BASE_IMAGE=voipmonitor/vllm:infernal-invocation-vllm3226eb7-b12x1584743-fi1ac6942-cu133-torch213-20260812-r4@sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6
FROM ${BASE_IMAGE}

ARG BASE_VLLM_TREE
ARG CANDIDATE_VLLM_TREE
ARG CANDIDATE_PARSER_SOURCE_ID
ARG OVERLAY_PATCH_SHA256
ARG CACHE_FINGERPRINT

USER root
COPY r4-v4-correctness.patch /tmp/r4-v4-correctness.patch

# Refuse a tag-compatible but source-incompatible base. The r4 image retains
# its complete source checkout and Git metadata at this absolute path.
RUN set -eux; \
    source_root=/opt/infernal-invocation/vllm; \
    test -d "${source_root}/.git"; \
    test "$(git -C "${source_root}" write-tree)" = "${BASE_VLLM_TREE}"; \
    echo "${OVERLAY_PATCH_SHA256}  /tmp/r4-v4-correctness.patch" | sha256sum -c -; \
    git -C "${source_root}" update-index --refresh -- \
      vllm/models/deepseek_v4/sparse_mla.py \
      vllm/parser/deepseek_v4.py \
      vllm/parser/engine/parser_engine.py \
      vllm/parser/engine/parser_engine_config.py \
      vllm/parser/engine/streaming_parser_engine.py \
      vllm/tool_parsers/utils.py; \
    git -C "${source_root}" apply --check /tmp/r4-v4-correctness.patch; \
    git -C "${source_root}" apply --index /tmp/r4-v4-correctness.patch; \
    git -C "${source_root}" diff --cached --check; \
    test "$(git -C "${source_root}" write-tree)" = "${CANDIDATE_VLLM_TREE}"; \
    python -c 'import importlib.util, pathlib; origin=pathlib.Path(importlib.util.find_spec("vllm.parser.deepseek_v4").origin).resolve(); assert origin.is_relative_to(pathlib.Path("/opt/infernal-invocation/vllm"))'; \
    rm /tmp/r4-v4-correctness.patch

# Never reuse r4's JIT/autotune cache namespace for a changed Python source
# tree. Every inherited cache variable used by the runtime is rebound.
ENV LOCAL_INFERENCE_CACHE_FINGERPRINT=${CACHE_FINGERPRINT} \
    XDG_CACHE_HOME=/cache/jit/${CACHE_FINGERPRINT} \
    VLLM_CACHE_ROOT=/cache/jit/${CACHE_FINGERPRINT}/vllm \
    VLLM_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/vllm \
    TRITON_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/triton \
    TORCH_EXTENSIONS_DIR=/cache/jit/${CACHE_FINGERPRINT}/torch-extensions \
    TORCHINDUCTOR_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/torchinductor \
    VLLM_FLASHINFER_AUTOTUNE_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/flashinfer-autotune \
    FLASHINFER_WORKSPACE_BASE=/cache/jit/${CACHE_FINGERPRINT}/flashinfer \
    TVM_FFI_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/tvm-ffi \
    TVM_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/tvm \
    TILELANG_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/tilelang \
    CUTE_DSL_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/cute-dsl \
    B12X_CUTE_COMPILE_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/b12x/cute \
    B12X_COMPILE_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/b12x/compile \
    SPARKINFER_COMPILE_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/b12x/compile \
    DG_JIT_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/deep-gemm \
    MM_SPARSE_ATTN_AOT_CACHE=/cache/jit/${CACHE_FINGERPRINT}/minfer/mm-sparse-attn \
    MINFER_FMHA_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/minfer/fmha-sm120 \
    NUMBA_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/numba \
    CUDA_CACHE_PATH=/cache/jit/${CACHE_FINGERPRINT}/cuda \
    CUPY_CACHE_DIR=/cache/jit/${CACHE_FINGERPRINT}/cupy

LABEL org.opencontainers.image.base.digest="sha256:21f048058375ccf00ea555f37addad326a7ee33bc2b4699ae53370f25af4ecb6" \
    local-inference.overlay.name="infernal-r5-v4-correctness" \
    local-inference.overlay.patch-sha256="${OVERLAY_PATCH_SHA256}" \
    local-inference.vllm.base-integration.tree="${BASE_VLLM_TREE}" \
    local-inference.vllm.integration.tree="${CANDIDATE_VLLM_TREE}" \
    local-inference.vllm.parser-source-id="${CANDIDATE_PARSER_SOURCE_ID}" \
    local-inference.cache.fingerprint="${CACHE_FINGERPRINT}"
