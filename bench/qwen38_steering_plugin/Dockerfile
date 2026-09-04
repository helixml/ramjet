ARG VLLM_IMAGE=vllm/vllm-openai@sha256:0aea30240f3e3d9ffae8526643950e170eb5fa07fc427016a9dd90892afa2aa3
FROM ${VLLM_IMAGE}

COPY qwen38_steering-0.3.0-py3-none-any.whl /tmp/qwen38_steering-0.3.0-py3-none-any.whl
RUN python3 -m pip install --no-cache-dir --no-deps \
      /tmp/qwen38_steering-0.3.0-py3-none-any.whl \
    && rm /tmp/qwen38_steering-0.3.0-py3-none-any.whl
