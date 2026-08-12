#!/usr/bin/env python3
"""Compare vLLM /tokenize output with prompt usage from real chat requests.

The script never prints prompts or token IDs. It checks token-ID determinism in
memory and emits only per-case counts and booleans.

Usage: tokenizer_parity.py BASE MODEL
Set BENCH_TOKEN (or VLLM_API_KEY) for authenticated engines.
"""

import copy
import json
import os
import sys
import urllib.error
import urllib.request


BASE = sys.argv[1].rstrip("/")
MODEL = sys.argv[2]
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
            {"role": "assistant", "content": "A cached block must match its token prefix."},
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
    "tool_history": {
        "messages": [
            {"role": "user", "content": "Read node06 status."},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "deployment_status",
                            "arguments": '{"node":"node06"}',
                        },
                    }
                ],
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "healthy"},
            {"role": "user", "content": "Summarize that result."},
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "deployment_status",
                    "description": "Return deployment health.",
                    "parameters": {"type": "object"},
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
    "reasoning_max": {
        "messages": [{"role": "user", "content": "Compare two routing scores."}],
        "reasoning_effort": "max",
    },
    "thinking_disabled": {
        "messages": [{"role": "user", "content": "Return one short sentence."}],
        "chat_template_kwargs": {"thinking": False},
    },
    "normalized_content": {
        "messages": [{"role": "user", "content": "first part; second part"}],
    },
}


def post(path, payload):
    request = urllib.request.Request(
        BASE + path,
        data=json.dumps(payload, separators=(",", ":")).encode(),
        headers={
            "Authorization": "Bearer " + TOKEN,
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        detail = error.read(4096).decode(errors="replace")
        raise RuntimeError(f"{path} returned HTTP {error.code}: {detail}") from error


def tokenization_payload(fields):
    payload = copy.deepcopy(fields)
    kwargs = dict(payload.get("chat_template_kwargs") or {})
    for key in ("documents", "reasoning_effort"):
        value = payload.get(key)
        if value is not None and value != "auto":
            kwargs[key] = value
    effort = payload.get("reasoning_effort")
    if effort is not None and "enable_thinking" not in kwargs:
        kwargs["enable_thinking"] = effort != "none"
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


failures = 0
for name, fields in CASES.items():
    completion_request = {
        "model": MODEL,
        "max_tokens": 1,
        "temperature": 0,
        **copy.deepcopy(fields),
    }
    completion = post("/v1/chat/completions", completion_request)
    usage_count = int(completion["usage"]["prompt_tokens"])

    tokenize_request = tokenization_payload(fields)
    first = post("/tokenize", tokenize_request)
    second = post("/tokenize", tokenize_request)
    tokenize_count = int(first["count"])
    ids_stable = first["tokens"] == second["tokens"]
    count_matches = tokenize_count == usage_count == len(first["tokens"])
    passed = ids_stable and count_matches
    failures += not passed
    print(
        json.dumps(
            {
                "case": name,
                "ids_stable": ids_stable,
                "match": count_matches,
                "prompt_tokens": usage_count,
                "tokenize_tokens": tokenize_count,
            },
            sort_keys=True,
        )
    )

raise SystemExit(1 if failures else 0)
