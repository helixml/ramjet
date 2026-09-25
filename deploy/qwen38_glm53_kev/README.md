# Qwen + GLM + Kev System One on node06

This deployment keeps one Ramjet instance in front of four upstreams:

- Qwen3.8-Flash-Next TP4 on GPUs 0-3 (`openai`).
- Two GLM-5.3-Flash TP2 replicas on GPUs 4-7 (`openai`).
- Kev-0.8B co-located on GPU 3 (`systemone`).

`RJ_UPSTREAM_MODELS` and `RJ_UPSTREAM_APIS` are intersected before dispatch.
OpenAI requests cannot reach Kev, and `/v1/systemone` requests cannot reach
Qwen or GLM. Public `GET /v1/models` stays OpenAI-compatible and lists only
Qwen and GLM; Kev's private readiness probe validates its native
`{"models": [...]}` response.

Kev is built from source commit
`5e94a28818cfd3d0ec9b8bca046dc8db0d79a704`, serves adapter revision
`54f4f8777356cd5bbbb6c6919c657f26e6f2f6d8`, and uses the base revision
recorded by that checkpoint. The derivative image changes only the bind host:
upstream's loopback default remains intact, while `KEV_HOST=0.0.0.0` binds
inside `ramjet_kev_systemone`, a fixed internal-only Docker network. No Kev
port is published. The runtime is non-root, read-only, and offline after the
pinned Hugging Face cache has been populated.

Request bodies of at least `RJ_ROUTE_LONG_PROMPT_BYTES` (default 600,000
bytes, roughly 150k tokens) route only to `glm53sm120-c`, the long-prompt
lane, while it is serving. A single ~310k-token GLM prefill takes about 57s
and evicts every other cached prefix on its replica, so confining it keeps
`glm53sm120-b`'s cache and latency intact. If the lane is unavailable the
request falls back to ordinary routing. Shorter requests still use both GLM
replicas, and Qwen is unaffected. Set `RJ_ROUTE_LONG_PROMPT_BYTES=0` to turn
the lane off without editing the file; `ramjet_route_long_prompt_total`
counts `lane` versus `fallback` decisions per upstream. The lane needs an LB
image that includes it; older images ignore both variables.

## Validate and build

```bash
python3 deploy/qwen38_glm53_kev/validate-compose.py
bash -n deploy/qwen38_glm53_kev/node06-rollout.sh
KEV_SOURCE_DIR=/path/to/kev deploy/qwen38_glm53_kev/build-kev-image.sh
```

The Kev builder requires a clean checkout at the exact source revision. Push
both candidate images, resolve their registry digests, and use only
`tag@sha256:digest` references for rollout.

## Rollout and rollback

Copy this directory to node06 and run `node06-rollout.sh` through the node's
thermal guard with `LB_IMAGE` and `KEV_IMAGE` set. The script holds the common
deployment lock, starts and directly warms Kev, proves a Ramjet canary on
alternate loopback ports, verifies both API families and cross-family 404s,
then replaces only the stateless LB. Qwen and GLM are never recreated.

The successful script prints the stopped, byte-identical rollback container.
To roll back, remove the new `ds4-loadbalancer`, rename that container back to
`ds4-loadbalancer`, start it, and bring down the
`qwen38_glm53_kev_runtime` Compose project. Keep the model cache; it contains
only public pinned model artifacts and makes a later retry fast.
