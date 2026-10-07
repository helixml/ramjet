#!/usr/bin/env python3
"""Fail-closed semantic validation for the 8x H200 DeepSeek-V4.1-Flash deployment."""

import json
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
SGLANG_IMAGE = "lmsysorg/sglang@sha256:b1259f3ea3275f66237c498ea388919729018bc9f01c3d638391e06e2cf3f469"
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.8.0@sha256:"
    "fe432bbca183d2a457a7713fb150ea5ee36aba7a13f92280ef3ec7195ec23673"
)
MODEL_REVISION = "2cba9e42aa026125f3ed06c6d98c1db82f7ca027"
DSPARK = "--speculative-algorithm DSPARK --speculative-dspark-block-size 5"
PATCHES = ("patch_bf16_dense.py", "patch_numa_preferred.py")
ENGINES = {
    "dsv41-a": {"port": "8070", "devices": ["0", "1", "2", "3"], "tp": "4", "cache": "dsv41-a", "numa": "1 1 2 3", "host_engram": "1"},
    "dsv41-b": {"port": "8071", "devices": ["4", "5", "6", "7"], "tp": "4", "cache": "dsv41-b", "numa": "4 5 6 7", "host_engram": "1"},
    "dsv41-tp8": {
        "port": "8072",
        "devices": [str(i) for i in range(8)],
        "tp": "8",
        "cache": "dsv41-tp8",
        "numa": "1 1 2 3 4 5 6 7",
        "host_engram": "0",
    },
}


def fail(message):
    raise SystemExit(f"dsv41 H200 compose validation failed: {message}")


def render(*profile):
    # Validate the committed defaults, not whatever the caller's shell exports.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("DS_", "RJ_", "LB_", "SGLANG_"))}
    env.update(MODEL_DIR="/models-under-test", CACHE_ROOT="/cache-under-test")
    rendered = subprocess.run(
        ["docker", "compose", "--env-file", os.devnull, "-f", str(COMPOSE), *profile, "config", "--format", "json"],
        cwd=ROOT,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(rendered.stdout)["services"]


default = render()
if set(default) != {"dsv41-a", "dsv41-b", "ramjet"}:
    fail("the default render must be exactly two TP4 replicas and ramjet")
with_tp8 = render("--profile", "tp8")
if set(with_tp8) != {"dsv41-a", "dsv41-b", "dsv41-tp8", "ramjet"}:
    fail("the tp8 engine must exist only behind its profile")

for patch in PATCHES:
    if not (ROOT / "patches" / patch).is_file():
        fail(f"patches/{patch} is missing")

caches = set()
for name, expected in ENGINES.items():
    service = with_tp8[name]
    if service.get("image") != SGLANG_IMAGE or service.get("pull_policy") != "never":
        fail(f"{name} must run the reviewed SGLang v0.5.21 digest with pull_policy never")
    if service.get("labels", {}).get("ai.ramjet.model.revision") != MODEL_REVISION:
        fail(f"{name} does not identify the pinned checkpoint revision")
    # SGLang names a /dev/shm segment after its own PID; a shared host /dev/shm
    # let two replicas cross-wire and one never ran a request.
    if service.get("ipc") == "host":
        fail(f"{name} must keep a private /dev/shm (no ipc: host)")
    env = service.get("environment", {})
    if env.get("DS_TP") != expected["tp"] or env.get("DS_EP") != expected["tp"]:
        fail(f"{name} must use TP/EP {expected['tp']}")
    if env.get("DS_NUMA_NODES") != expected["numa"] or "SYS_NICE" not in service.get("cap_add", []):
        fail(f"{name} must bind TP ranks to NUMA nodes {expected['numa']} with SYS_NICE")
    if env.get("SGLANG_ENABLE_DSV41_ENGRAM_HOST_TABLE") != expected["host_engram"]:
        fail(f"{name}: Engram host table must be {expected['host_engram']}")
    if expected["host_engram"] == "1" and env.get("SGLANG_NUMA_MEM_PREFERRED") != "1":
        fail(f"{name}: a hard NUMA memory bind OOM-kills rank 0 with the host Engram table")
    if env.get("SGLANG_BLOCK_FP8_DEQUANT_BF16") != "1":
        fail(f"{name}: the 32x32 block-FP8 dense layers must run as BF16 on Hopper")
    if env.get("DS_PATCHES", "").split() != list(PATCHES):
        fail(f"{name} must apply {' '.join(PATCHES)} at start")
    if env.get("DS_SPEC_ARGS") != DSPARK:
        fail(f"{name} must default to DSpark with block size 5")
    if env.get("DS_CONTEXT_LENGTH") != "262144":
        fail(f"{name}: DSpark OOMs in graph capture at the native 1M context at these defaults")
    if env.get("DS_MEM_FRACTION") != "0.75" or env.get("DS_CUDA_GRAPH_MAX_BS") != "64":
        fail(f"{name} must default to memory fraction 0.75 and graph cap 64, the measured cell")
    if service.get("init") is not True or service.get("ulimits", {}).get("memlock") != -1:
        fail(f"{name} needs init and an unlimited memlock")
    if env.get("DS_MOE_PRECISION") != "fp8":
        fail(f"{name} must default to FP8 MoE activations (W4A8)")
    if env.get("DS_TOKENIZER_WORKERS") != "4" or env.get("SGLANG_UVICORN_WORKER_HEALTHCHECK_TIMEOUT") != "60":
        fail(f"{name}: keep 4 tokenizer workers and a 60s worker health check")
    if env.get("SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION") != "0":
        fail(f"{name}: /health must not generate, or probes time out under load")
    if env.get("DS_EXTRA_ARGS"):
        fail(f"{name} must not carry extra engine flags by default")
    command = " ".join(service.get("command", []))
    if "--enable-dp-attention" in command:
        fail(f"{name}: DP attention with DSpark hangs after weight load")
    for flag in (
        "for p in $$DS_PATCHES; do python3 /opt/dsv41-patches/$$p",
        "--tp-size=$$DS_TP --ep-size=$$DS_EP",
        "--context-length=$$DS_CONTEXT_LENGTH",
        "--mem-fraction-static=$$DS_MEM_FRACTION",
        "--cuda-graph-max-bs-decode=$$DS_CUDA_GRAPH_MAX_BS",
        "--enable-decoder-swa-bounded-replay",
        "--attention-backend=dsv4",
        "--moe-runner-backend=flashinfer_mxfp4",
        "--flashinfer-mxfp4-moe-precision=$$DS_MOE_PRECISION",
        "--enable-cache-report",
        "--numa-node $$DS_NUMA_NODES",
        "--tokenizer-worker-num $$DS_TOKENIZER_WORKERS",
        "$$DS_SPEC_ARGS",
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
    if not mounts.get("/models/dsv41", {}).get("read_only"):
        fail(f"{name} must mount the model read-only")
    if not mounts.get("/opt/dsv41-patches", {}).get("read_only"):
        fail(f"{name} must mount the patches read-only")
    cache = mounts.get("/root/.cache", {}).get("source", "")
    if not cache.endswith("/" + expected["cache"]) or cache in caches:
        fail(f"{name} needs its own JIT cache directory")
    caches.add(cache)

if set(ENGINES["dsv41-a"]["devices"]) & set(ENGINES["dsv41-b"]["devices"]):
    fail("the TP4 replicas must not share GPUs")

lb = default["ramjet"]
if lb.get("image") != LB_IMAGE:
    fail("ramjet must run the pinned release image")
env = lb.get("environment", {})
expected_env = {
    "RJ_UPSTREAM": "http://dsv41-a:8000,http://dsv41-b:8000",
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
if env.get("RJ_ROUTE_LONG_PROMPT_BYTES") or env.get("RJ_ROUTE_LONG_PROMPT_UPSTREAMS"):
    fail("the 1M lane is opt-in; the default replicas both serve 262k")
for port in lb.get("ports", []):
    if port.get("host_ip") != "127.0.0.1":
        fail("ramjet must publish only on loopback")
print("dsv41 H200 compose validation passed")
