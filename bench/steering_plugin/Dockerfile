ARG VLLM_IMAGE=vllm/vllm-openai@sha256:5f1142f7ceea906a61bc46c76b1f1d562c2d4898f604e1f6cd3620ceafd9ce93
FROM ${VLLM_IMAGE}

COPY qwen38_steering-0.4.1-py3-none-any.whl /tmp/qwen38_steering-0.4.1-py3-none-any.whl
RUN python3 -m pip install --no-cache-dir --no-deps \
      /tmp/qwen38_steering-0.4.1-py3-none-any.whl \
    && rm /tmp/qwen38_steering-0.4.1-py3-none-any.whl
