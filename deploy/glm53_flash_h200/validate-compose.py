#!/usr/bin/env python3
"""Fail-closed semantic validation for the 8x H200 GLM-5.3-Flash deployment."""

import json
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
SGLANG_IMAGE = "lmsysorg/sglang@sha256:06e4f2ed21afde4ff513cda65070124e727ba23ccaeff7712b8c40e1097d611f"
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:rust-a524263@sha256:"
    "7f874182ee28dca1764454107647fcba67696292481d8c068b5ca9ab8ce3092c"
)
MODEL_REVISION = "eb9eb208eb0d988989d07a6a12d0fdeb5f52574a"
MTP = "--speculative-algorithm EAGLE --speculative-num-steps 3 --speculative-eagle-topk 1 --speculative-num-draft-tokens 4"
ENGINES = {
    "glm53-a": {"port": "8070", "devices": ["0", "1", "2", "3"], "tp": "4", "cache": "sglang-a", "numa": "0 1 2 3"},
    "glm53-b": {"port": "8071", "devices": ["4", "5", "6", "7"], "tp": "4", "cache": "sglang-b", "numa": "4 5 6 7"},
    "glm53-tp8": {
        "port": "8072",
        "devices": [str(i) for i in range(8)],
        "tp": "8",
        "cache": "sglang-tp8",
        "numa": "0 1 2 3 4 5 6 7",
    },
}


def fail(message):
    raise SystemExit(f"glm53 H200 compose validation failed: {message}")


def render(*profile):
    # Validate the committed defaults, not whatever the caller's shell exports.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GLM_", "RJ_", "LB_", "SGLANG_"))}
    env.update(MODEL_DIR="/models-under-test", CACHE_ROOT="/cache-under-test")
    rendered = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE), *profile, "config", "--format", "json"],
        cwd=ROOT,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(rendered.stdout)["services"]


default = render()
if set(default) != {"glm53-a", "glm53-b", "ramjet"}:
    fail("the default render must be exactly two TP4 replicas and ramjet")
with_tp8 = render("--profile", "tp8")
if set(with_tp8) != {"glm53-a", "glm53-b", "glm53-tp8", "ramjet"}:
    fail("the tp8 control must exist only behind its profile")

caches = set()
for name, expected in ENGINES.items():
    service = with_tp8[name]
    if service.get("image") != SGLANG_IMAGE or service.get("pull_policy") != "never":
        fail(f"{name} must run the reviewed SGLang v0.5.20 digest with pull_policy never")
    if service.get("labels", {}).get("ai.ramjet.model.revision") != MODEL_REVISION:
        fail(f"{name} does not identify the pinned checkpoint revision")
    env = service.get("environment", {})
    if env.get("GLM_TP") != expected["tp"] or env.get("GLM_EP") != expected["tp"]:
        fail(f"{name} must use TP/EP {expected['tp']}")
    # Each GPU hangs off its own host NUMA node (GPU i on node i, sockets 0-3
    # and 4-7). Without SYS_NICE SGLang silently skips the bind.
    if env.get("GLM_NUMA_NODES") != expected["numa"] or "SYS_NICE" not in service.get("cap_add", []):
        fail(f"{name} must bind TP ranks to NUMA nodes {expected['numa']} with SYS_NICE")
    if env.get("GLM_KV_DTYPE") != "bfloat16":
        fail(f"{name}: FP8 KV is not a valid SM90 combination for this NoPE sparse MLA")
    if env.get("GLM_SPEC_ARGS") != MTP:
        fail(f"{name} must default to the measured EAGLE 3/1/4 MTP profile")
    if env.get("GLM_HICACHE_ARGS") != "--enable-hierarchical-cache --hicache-size 48":
        fail(f"{name} must default to the measured 48GB-per-rank host KV tier")
    if env.get("GLM_TOKENIZER_WORKERS") != "4" or env.get("SGLANG_UVICORN_WORKER_HEALTHCHECK_TIMEOUT") != "60":
        fail(f"{name}: one tokenizer process saturates under agent load; keep 4 workers and a 60s worker health check")
    if env.get("SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION") != "0":
        fail(f"{name}: /health must not generate, or probes time out under load")
    if env.get("GLM_EXTRA_ARGS"):
        fail(f"{name} must not carry extra engine flags by default")
    command = " ".join(service.get("command", []))
    if "--enable-mixed-chunk" in command or "--speculative-adaptive" in command:
        fail(f"{name}: mixed-chunk OOM-crashed and adaptive MTP measured no gain")
    for flag in (
        "--tool-call-parser=glm47",
        "--reasoning-parser=glm45",
        "--enable-cache-report",
        "--numa-node $$GLM_NUMA_NODES",
        "--tokenizer-worker-num $$GLM_TOKENIZER_WORKERS",
        "$$GLM_SPEC_ARGS",
        "$$GLM_HICACHE_ARGS",
    ):
        if flag not in command:
            fail(f"{name} command lacks {flag}")
    devices = service["deploy"]["resources"]["reservations"]["devices"][0]["device_ids"]
    if devices != expected["devices"]:
        fail(f"{name} must own GPUs {expected['devices']}")
    ports = service.get("ports", [])
    if len(ports) != 1 or ports[0].get("host_ip") != "127.0.0.1" or ports[0].get("published") != expected["port"]:
        fail(f"{name} must publish only loopback :{expected['port']}")
    mounts = {volume["target"]: volume for volume in service.get("volumes", [])}
    if not mounts.get("/models/glm53", {}).get("read_only"):
        fail(f"{name} must mount the model read-only")
    cache = mounts.get("/root/.cache", {}).get("source", "")
    if not cache.endswith("/" + expected["cache"]) or cache in caches:
        fail(f"{name} needs its own JIT cache directory")
    caches.add(cache)

if set(ENGINES["glm53-a"]["devices"]) & set(ENGINES["glm53-b"]["devices"]):
    fail("the TP4 replicas must not share GPUs")

lb = default["ramjet"]
if lb.get("image") != LB_IMAGE:
    fail("ramjet must run the pinned post-#296 main image")
env = lb.get("environment", {})
expected_env = {
    "RJ_UPSTREAM": "http://glm53-a:8000,http://glm53-b:8000",
    "RJ_AFFINITY": "prefix",
    "RJ_ROUTE_AFFINITY_BASIS": "marginal",
    "RJ_TOKENIZER_MODE": "off",
    "RJ_EXACT_ROUTE_MODE": "off",
    "RJ_KV_EVENT_MODE": "off",
    "RJ_SNAPSHOT_ROUTE_MODE": "off",
    "RJ_IDLE_DRAIN_MODE": "off",
}
for key, value in expected_env.items():
    if env.get(key) != value:
        fail(f"ramjet {key} must be {value}")
for port in lb.get("ports", []):
    if port.get("host_ip") != "127.0.0.1":
        fail("ramjet must publish only on loopback")
print("glm53 H200 compose validation passed")
