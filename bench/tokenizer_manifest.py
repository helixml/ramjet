#!/usr/bin/env python3
"""Generate a privacy-safe renderer compatibility manifest from vLLM.

The emitted JSON contains synthetic requests, token counts, and SHA-256
digests of token-ID vectors. Raw token IDs are never printed or persisted.

Usage: tokenizer_manifest.py BASE MODEL ENGINE_IMAGE_DIGEST TOKENIZER_PATH
Set BENCH_TOKEN (or VLLM_API_KEY) for authenticated engines.
"""

import copy
import hashlib
import json
import os
import struct
import sys
import urllib.error
import urllib.request


if len(sys.argv) != 5:
    raise SystemExit(
        "usage: tokenizer_manifest.py BASE MODEL ENGINE_IMAGE_DIGEST TOKENIZER_PATH"
    )

BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
ENGINE_IMAGE_DIGEST = sys.argv[3]
TOKENIZER_PATH = sys.argv[4]
TOKEN = os.environ.get("BENCH_TOKEN") or os.environ.get("VLLM_API_KEY")
if not TOKEN:
    raise SystemExit("set BENCH_TOKEN or VLLM_API_KEY")

CASES = {
    "plain": {
        "messages": [{"role": "user", "content": "Explain prefix caching briefly."}],
    },
    "system_multiturn": {
        "messages": [
            {"role": "system", "content": "Answer as a concise systems engineer."},
            {"role": "user", "content": "Name one cache invariant."},
            {
                "role": "assistant",
                "content": "A cached block must match its token prefix.",
            },
            {"role": "user", "content": "Now name one recovery invariant."},
        ],
    },
    "tools_declared": {
        "messages": [{"role": "user", "content": "Read the deployment status."}],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "deployment_status",
                    "description": "Return deployment health.",
                    "parameters": {
                        "type": "object",
                        "properties": {"node": {"type": "string"}},
                        "required": ["node"],
                    },
                },
            }
        ],
    },
    "reasoning_high": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "high",
    },
    "reasoning_none": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "none",
    },
    "reasoning_minimal": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "minimal",
    },
    "reasoning_low": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "low",
    },
    "reasoning_medium": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "medium",
    },
    "thinking_disabled": {
        "messages": [{"role": "user", "content": "Return one short sentence."}],
        "chat_template_kwargs": {"thinking": False},
    },
    "normalized_content": {
        "messages": [{"role": "user", "content": "first part; second part"}],
    },
}

ADMITTED_REQUEST_CLASSES = [
    "plain",
    "system_multiturn",
    "tools_declared",
    "reasoning_high",
    "reasoning_none",
    "reasoning_minimal",
    "reasoning_low",
    "reasoning_medium",
    "thinking_disabled",
]


def request(method, path, payload=None):
    body = None
    headers = {"Authorization": "Bearer " + TOKEN}
    if payload is not None:
        body = json.dumps(payload, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    call = urllib.request.Request(BASE + path, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(call, timeout=120) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read(4096).decode(errors="replace")
        raise RuntimeError(f"{path} returned HTTP {error.code}: {detail}") from error


def tokenization_payload(fields):
    payload = copy.deepcopy(fields)
    kwargs = dict(payload.get("chat_template_kwargs") or {})
    effort = payload.get("reasoning_effort")
    if effort is not None:
        kwargs["reasoning_effort"] = effort
        kwargs.setdefault("enable_thinking", effort != "none")
    if kwargs:
        payload["chat_template_kwargs"] = kwargs
    payload.update(
        {
            "model": MODEL,
            "add_generation_prompt": True,
            "return_token_strs": False,
        }
    )
    return payload


def token_ids_sha256(tokens):
    digest = hashlib.sha256()
    for token in tokens:
        digest.update(struct.pack(">I", int(token)))
    return digest.hexdigest()


with open(TOKENIZER_PATH, "rb") as tokenizer_file:
    tokenizer_sha256 = hashlib.file_digest(tokenizer_file, "sha256").hexdigest()

models = request("GET", "/v1/models")
matching_models = [entry for entry in models["data"] if entry.get("id") == MODEL]
if len(matching_models) != 1:
    raise SystemExit(f"expected exactly one {MODEL!r} model")
model = matching_models[0]
version = request("GET", "/version")["version"]

goldens = []
for name, fields in CASES.items():
    payload = tokenization_payload(fields)
    first = request("POST", "/tokenize", payload)
    second = request("POST", "/tokenize", payload)
    tokens = first["tokens"]
    if tokens != second["tokens"] or first["count"] != len(tokens):
        raise SystemExit(f"unstable or malformed tokenization for {name}")
    goldens.append(
        {
            "name": name,
            "endpoint": "chat",
            "request": payload,
            "token_count": len(tokens),
            "token_ids_sha256": token_ids_sha256(tokens),
        }
    )

manifest = {
    "schema_version": 1,
    "model": {
        "id": model["id"],
        "root": model["root"],
        "max_model_len": model["max_model_len"],
    },
    "engine": {
        "version": version,
        "image_digest": ENGINE_IMAGE_DIGEST,
    },
    "tokenizer": {"sha256": tokenizer_sha256},
    "renderer": {"profile": "deepseek-v4-r34"},
    "admitted_request_classes": ADMITTED_REQUEST_CLASSES,
    "goldens": goldens,
}
print(json.dumps(manifest, indent=2, sort_keys=True))
