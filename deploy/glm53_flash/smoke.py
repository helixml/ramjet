#!/usr/bin/env python3
"""Bounded GLM OpenAI/tool smoke that never prints generated text."""

from __future__ import annotations

import argparse
import json
import time
import urllib.request

from metrics import reconcile, snapshot


def request(base: str, body: dict) -> tuple[dict, float]:
    started = time.perf_counter()
    call = urllib.request.Request(
        base.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer smoke"},
    )
    with urllib.request.urlopen(call, timeout=180) as response:
        if response.status != 200:
            raise ValueError(f"unexpected HTTP status {response.status}")
        payload = json.load(response)
    return payload, time.perf_counter() - started


def usage(payload: dict) -> dict[str, int]:
    result = payload.get("usage")
    if not isinstance(result, dict):
        raise ValueError("response has no usage object")
    values = {}
    for key in ("prompt_tokens", "completion_tokens", "total_tokens"):
        value = result.get(key)
        if not isinstance(value, int) or value <= 0:
            raise ValueError(f"response has invalid {key}")
        values[key] = value
    return values


def message(payload: dict) -> dict:
    choices = payload.get("choices")
    if not isinstance(choices, list) or len(choices) != 1:
        raise ValueError("response does not contain exactly one choice")
    result = choices[0].get("message")
    if not isinstance(result, dict):
        raise ValueError("response choice has no message")
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("base")
    parser.add_argument("--model", default="glm-5.3-flash")
    parser.add_argument("--metrics")
    args = parser.parse_args()
    native_before = snapshot(args.metrics) if args.metrics else None
    common = {
        "model": args.model,
        "temperature": 0,
        "max_tokens": 128,
        "chat_template_kwargs": {"reasoning_effort": "low"},
    }

    basic, basic_seconds = request(
        args.base,
        {
            **common,
            "messages": [
                {
                    "role": "user",
                    "content": "Reply with exactly the word READY and no punctuation.",
                }
            ],
        },
    )
    basic_message = message(basic)
    content = basic_message.get("content")
    if not isinstance(content, str) or content.strip() != "READY":
        raise ValueError("bounded deterministic text check failed")

    tool, tool_seconds = request(
        args.base,
        {
            **common,
            "messages": [
                {
                    "role": "user",
                    "content": "Use the add tool to calculate 19 plus 23. Do not answer directly.",
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "add",
                        "description": "Add two integers.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "a": {"type": "integer"},
                                "b": {"type": "integer"},
                            },
                            "required": ["a", "b"],
                            "additionalProperties": False,
                        },
                    },
                }
            ],
            "tool_choice": "auto",
        },
    )
    tool_message = message(tool)
    calls = tool_message.get("tool_calls")
    if not isinstance(calls, list) or len(calls) != 1:
        raise ValueError("response does not contain exactly one tool call")
    function = calls[0].get("function")
    if not isinstance(function, dict) or function.get("name") != "add":
        raise ValueError("response selected the wrong tool")
    arguments = json.loads(function.get("arguments", ""))
    if arguments != {"a": 19, "b": 23} and arguments != {"a": 23, "b": 19}:
        raise ValueError("response emitted incorrect tool arguments")

    basic_usage = usage(basic)
    tool_usage = usage(tool)
    native_delta = None
    if native_before is not None:
        native_delta = reconcile(
            native_before,
            snapshot(args.metrics),
            {
                "requests": 2,
                "prompt_tokens": basic_usage["prompt_tokens"]
                + tool_usage["prompt_tokens"],
                "completion_tokens": basic_usage["completion_tokens"]
                + tool_usage["completion_tokens"],
            },
        )

    print(
        json.dumps(
            {
                "basic": {
                    "ok": True,
                    "seconds": round(basic_seconds, 3),
                    **basic_usage,
                },
                "native_delta": native_delta,
                "tool": {
                    "ok": True,
                    "seconds": round(tool_seconds, 3),
                    **tool_usage,
                },
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
