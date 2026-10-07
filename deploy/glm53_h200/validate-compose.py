#!/usr/bin/env python3
"""Fail-closed semantic validation for the 8x H200 GLM-5.3 (753B) deployment."""

import json
import os
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parent
COMPOSE = ROOT / "docker-compose.yaml"
SGLANG_IMAGE = "lmsysorg/sglang@sha256:06e4f2ed21afde4ff513cda65070124e727ba23ccaeff7712b8c40e1097d611f"
LB_IMAGE = (
    "ghcr.io/helixml/ramjet:v0.8.0@sha256:"
    "fe432bbca183d2a457a7713fb150ea5ee36aba7a13f92280ef3ec7195ec23673"
)
MODEL_REVISION = "aca966e4e02791568aa6a4ced368624b3d897f42"
MTP = "--speculative-algorithm EAGLE --speculative-num-steps 1 --speculative-eagle-topk 1 --speculative-num-draft-tokens 2"
HICACHE = (
    "--enable-hierarchical-cache --hicache-size 32 --hicache-write-policy write_through "
    "--hicache-mem-layout page_first --hicache-io-backend kernel"
)
RANKS = 8


def fail(message):
    raise SystemExit(f"glm53 H200 compose validation failed: {message}")


def render():
    # Validate the committed defaults, not whatever the caller's shell exports.
    env = {k: v for k, v in os.environ.items() if not k.startswith(("GLM_", "RJ_", "LB_", "SGLANG_"))}
    env.update(MODEL_DIR="/models-under-test", CACHE_ROOT="/cache-under-test")
    rendered = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE), "config", "--format", "json"],
        cwd=ROOT,
        env=env,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(rendered.stdout)["services"]


services = render()
if set(services) != {"glm53", "ramjet"}:
    fail("the render must be exactly one engine and ramjet")

engine = services["glm53"]
if engine.get("image") != SGLANG_IMAGE or engine.get("pull_policy") != "never":
    fail("glm53 must run the reviewed SGLang v0.5.20 digest with pull_policy never")
if engine.get("labels", {}).get("ai.ramjet.model.revision") != MODEL_REVISION:
    fail("glm53 does not identify the pinned checkpoint revision")
env = engine.get("environment", {})
command = " ".join(engine.get("command", []))
for flag in (
    "--tp-size=8 --dp-size=8 --enable-dp-attention --ep-size=8",
    "--kv-cache-dtype=fp8_e4m3",
    "--dsa-prefill-backend=flashmla_sparse_q8",
    "--dsa-decode-backend=flashmla_kv",
    "--schedule-policy=hrrn",
    "--tool-call-parser=glm47",
    "--reasoning-parser=glm45",
    "--enable-cache-report",
    "--numa-node $$GLM_NUMA_NODES",
    "--tokenizer-worker-num $$GLM_TOKENIZER_WORKERS",
    "$$GLM_SPEC_ARGS",
    "$$GLM_HICACHE_ARGS",
):
    if flag not in command:
        fail(f"glm53 command lacks {flag}")
# Each GPU hangs off its own host NUMA node. Without SYS_NICE SGLang silently
# skips the bind. Rank 0 uses node 1: GPU DMA into node 0's sub-4GiB window is
# misrouted on this VM (Xid 94).
numa = env.get("GLM_NUMA_NODES", "").split()
if numa != ["1", "1", "2", "3", "4", "5", "6", "7"] or "SYS_NICE" not in engine.get("cap_add", []):
    fail("glm53 must bind ranks to NUMA nodes 1 1 2 3 4 5 6 7 with SYS_NICE")
if env.get("GLM_MOE_A2A_BACKEND") != "deepep":
    fail("glm53 must default to the DeepEP all-to-all measured at 16-48 developers")
if env.get("GLM_SPEC_ARGS") != MTP:
    fail("glm53 must default to the measured EAGLE 1/1/2 MTP profile")
if env.get("GLM_HICACHE_ARGS") != HICACHE:
    fail("glm53 must default to the measured 32GB-per-rank host KV tier")
if env.get("GLM_MEM_FRACTION") != "0.85" or env.get("GLM_CHUNKED_PREFILL") != "32768":
    fail("glm53 must keep mem-fraction 0.85 and 32k prefill chunks (0.88/64k OOM-crashed)")
if env.get("GLM_MAX_RUNNING") != "128":
    fail("glm53 must keep 16 running requests per DP rank")
if env.get("GLM_TOKENIZER_WORKERS") != "4" or env.get("SGLANG_UVICORN_WORKER_HEALTHCHECK_TIMEOUT") != "60":
    fail("glm53: one tokenizer process saturates under agent load; keep 4 workers and a 60s worker health check")
if env.get("SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION") != "0":
    fail("glm53: /health must not generate, or probes time out under load")
if env.get("GLM_EXTRA_ARGS"):
    fail("glm53 must not carry extra engine flags by default")
devices = engine["deploy"]["resources"]["reservations"]["devices"][0]["device_ids"]
if devices != [str(i) for i in range(RANKS)]:
    fail("glm53 must own all eight GPUs")
ports = engine.get("ports", [])
if len(ports) != 1 or ports[0].get("host_ip") != "127.0.0.1" or ports[0].get("published") != "8073":
    fail("glm53 must publish only loopback :8073")
mounts = {volume["target"]: volume for volume in engine.get("volumes", [])}
if not mounts.get("/models/glm53", {}).get("read_only"):
    fail("glm53 must mount the model read-only")
if not mounts.get("/root/.cache", {}).get("source", "").endswith("/sglang-glm53"):
    fail("glm53 needs a persistent JIT cache directory")

lb = services["ramjet"]
if lb.get("image") != LB_IMAGE:
    fail("ramjet must run the pinned v0.7.0 release")
env = lb.get("environment", {})
upstreams = env.get("RJ_UPSTREAM", "").split(",")
if upstreams != ["http://glm53:8000"] * RANKS:
    fail("ramjet must list the engine once per DP rank")
if env.get("RJ_UPSTREAM_DP_RANKS", "").split(",") != [str(i) for i in range(RANKS)]:
    fail("ramjet must pin upstream i to DP rank i")
expected_env = {
    "RJ_AFFINITY": "prefix",
    "RJ_ROUTE_AFFINITY_BASIS": "relative",
    "RJ_UPSTREAM_RANK_PROBE": "on",
    "RJ_TOKENIZER_MODE": "off",
    "RJ_EXACT_ROUTE_MODE": "off",
    "RJ_KV_EVENT_MODE": "off",
    "RJ_SNAPSHOT_ROUTE_MODE": "off",
    "RJ_UPSTREAM_ADMISSION_MODE": "http",
    "RJ_IDLE_DRAIN_MODE": "off",
}
for key, value in expected_env.items():
    if env.get(key) != value:
        fail(f"ramjet {key} must be {value}")
for port in lb.get("ports", []):
    if port.get("host_ip") != "127.0.0.1":
        fail("ramjet must publish only on loopback")
print("glm53 H200 compose validation passed")
