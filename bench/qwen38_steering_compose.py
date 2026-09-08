#!/usr/bin/env python3
"""Derive a complete single-file Qwen steering experiment Compose file."""

from __future__ import annotations

import argparse
import os
import pathlib
import re


SERVICE_MARKER = "  qwen38flashnext-b:\n    <<: *vllm-engine\n    container_name: qwen38flashnext-b\n"
ADAPTIVE_RE = r"^      RJ_ADAPTIVE_CONFIG_PATH: /etc/ramjet/adaptive-config\.json\n"

def kv_lines(name: str, port: str) -> str:
    suffix = "5557" if port == "live" else "5558"
    return (
        rf"^      RJ_KV_EVENT_{name.upper()}_ENDPOINTS: "
        rf"(?:\$\{{RJ_KV_EVENT_{name.upper()}_ENDPOINTS:-)?"
        rf"tcp://qwen38flashnext-a:{suffix},tcp://qwen38flashnext-b:{suffix},"
        rf"tcp://qwen38flashnext-tp8:{suffix}\}}?\n"
    )


def kv_replacement(name: str, port: str) -> str:
    suffix = "5557" if port == "live" else "5558"
    return (
        f"      RJ_KV_EVENT_{name.upper()}_ENDPOINTS: "
        f"${{RJ_KV_EVENT_{name.upper()}_ENDPOINTS:-tcp://qwen38flashnext-a:{suffix},"
        f"tcp://qwen38flashnext-b:{suffix},tcp://qwen38flashnext-tp8:{suffix}}}\n"
    )


def render(source: str, args: argparse.Namespace) -> str:
    if source.count(SERVICE_MARKER) != 1:
        raise ValueError("canonical Compose has an unexpected engine-B shape")
    if (
        len(re.findall(kv_lines("live", "live"), source, re.M)) != 1
        or len(re.findall(kv_lines("replay", "replay"), source, re.M)) != 1
        or len(re.findall(ADAPTIVE_RE, source, re.M)) != 1
    ):
        raise ValueError("canonical Compose has an unexpected KV-event shape")
    if not args.image.startswith("qwen38-steering:"):
        raise ValueError("candidate image must be an explicit qwen38-steering tag")

    model_mount = getattr(args, "model_mount", None) or (
        "${MODEL_DIR:-/prod/models/Qwen/Qwen3.8-Flash-Next-FP8-bcd9f01ddc9c}"
        ":/workspace/model:ro"
    )
    cache_mount = getattr(args, "cache_mount", None) or (
        "${VLLM_CACHE_DIR:-/prod/engine-cache-vllm-qwen38flashnext}:/root/.cache"
    )
    extra = [
        f"    image: {args.image}",
        "    volumes:",
        f"      - {model_mount}",
        f"      - {cache_mount}",
        "    environment:",
        "      CUDA_DEVICE_ORDER: PCI_BUS_ID",
        '      CUDA_VISIBLE_DEVICES: "0,1,2,3"',
        "      VLLM_API_KEY: ${VLLM_API_KEY:-qwen-local}",
        '      VLLM_PLE_CPU_OFFLOAD: "0"',
        "      VLLM_PLUGINS: qwen38_steering",
    ]
    if args.mode == "capture":
        capture_dir = args.capture_dir.resolve()
        extra.insert(4, f"      - {capture_dir}:/steering-captures")
        extra.extend(
            [
                "      QWEN38_STEERING_CAPTURE_DIR: /steering-captures",
                "      QWEN38_STEERING_CAPTURE_ENABLE_FILE: /steering-captures/enabled",
            ]
        )
    else:
        vector = args.vector.resolve()
        if args.control_file is not None:
            control = args.control_file.resolve()
            if control.parent != vector.parent:
                raise ValueError("dynamic control and vector must share one directory")
            extra.insert(4, f"      - {vector.parent}:/steering:ro")
            extra.extend(
                [
                    f"      QWEN38_STEERING_VECTOR: /steering/{vector.name}",
                    f"      QWEN38_STEERING_CONTROL_FILE: /steering/{control.name}",
                ]
            )
        else:
            extra.insert(4, f"      - {vector}:/steering/vector.safetensors:ro")
            extra.extend(
                [
                    "      QWEN38_STEERING_VECTOR: /steering/vector.safetensors",
                    f'      QWEN38_STEERING_SCALE: "{args.scale:g}"',
                    f'      QWEN38_STEERING_LAYERS: "{args.layers}"',
                ]
            )
    replacement = SERVICE_MARKER + "\n".join(extra) + "\n"
    rendered = source.replace(SERVICE_MARKER, replacement)
    engine_start = rendered.index(SERVICE_MARKER)
    engine_end = rendered.index("\n  qwen38flashnext-tp8:", engine_start)
    engine = rendered[engine_start:engine_end]
    tp_marker = "      - --tensor-parallel-size=4\n"
    if engine.count(tp_marker) != 1:
        raise ValueError("canonical Compose has an unexpected engine-B command")
    engine = engine.replace(tp_marker, tp_marker + "      - --enforce-eager\n")
    rendered = rendered[:engine_start] + engine + rendered[engine_end:]
    rendered = re.sub(kv_lines("live", "live"), kv_replacement("live", "live"),
                      rendered, count=1, flags=re.M)
    rendered = re.sub(kv_lines("replay", "replay"), kv_replacement("replay", "replay"),
                      rendered, count=1, flags=re.M)
    return re.sub(ADAPTIVE_RE, "", rendered, count=1, flags=re.M)


def write_exclusive(path: pathlib.Path, content: str) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as handle:
            handle.write(content)
    except BaseException:
        path.unlink(missing_ok=True)
        raise


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("capture", "steer"))
    parser.add_argument("--source", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--capture-dir", type=pathlib.Path)
    parser.add_argument("--vector", type=pathlib.Path)
    parser.add_argument("--control-file", type=pathlib.Path)
    parser.add_argument("--scale", type=float, default=1.0)
    parser.add_argument("--layers", default="all")
    parser.add_argument("--model-mount", help="override the /workspace/model volume mount")
    parser.add_argument("--cache-mount", help="override the /root/.cache volume mount")
    args = parser.parse_args()
    if args.mode == "capture" and args.capture_dir is None:
        parser.error("capture mode requires --capture-dir")
    if args.mode == "steer" and args.vector is None:
        parser.error("steer mode requires --vector")
    if args.mode == "capture" and args.control_file is not None:
        parser.error("capture mode does not accept --control-file")
    source = args.source.read_text()
    write_exclusive(args.output, render(source, args))


if __name__ == "__main__":
    main()
