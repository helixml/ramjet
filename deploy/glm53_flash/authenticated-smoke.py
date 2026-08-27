#!/usr/bin/env python3
"""Privacy-safe authenticated Caddy -> Ramjet -> GLM smoke check."""

import argparse
import json
import pathlib
import re
import urllib.request


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--url",
        default="http://100.89.187.17/v1/chat/completions",
    )
    parser.add_argument("--model", default="glm-5.3-flash")
    parser.add_argument("--caddyfile", default="/etc/caddy/Caddyfile")
    args = parser.parse_args()

    caddyfile = pathlib.Path(args.caddyfile).read_text()
    token_match = re.search(r"Bearer ([A-Za-z0-9_-]+)", caddyfile)
    if token_match is None:
        raise SystemExit("Caddy bearer authority is missing")

    payload = {
        "model": args.model,
        "messages": [
            {
                "role": "user",
                "content": "Reply with exactly the word READY and no punctuation.",
            }
        ],
        "temperature": 0,
        "max_tokens": 8,
        "chat_template_kwargs": {"reasoning_effort": "low"},
    }
    request = urllib.request.Request(
        args.url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {token_match.group(1)}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        body = json.load(response)
        status = response.status
        routed = bool(response.headers.get("X-Ramjet-Upstream"))

    choices = body.get("choices", [])
    usage = body.get("usage", {})
    content = choices[0].get("message", {}).get("content") if choices else None
    finish_reason = choices[0].get("finish_reason") if choices else None
    checks = {
        "http_200": status == 200,
        "ramjet_route_header": routed,
        "model_matches": body.get("model") == args.model,
        "one_choice": len(choices) == 1,
        "exact_answer": isinstance(content, str) and content.strip() == "READY",
        "usage_present": all(
            isinstance(usage.get(key), int)
            for key in ("prompt_tokens", "completion_tokens", "total_tokens")
        ),
    }
    result = {
        "ok": all(checks.values()),
        "checks": checks,
        "finish_reason": finish_reason,
        "prompt_tokens": usage.get("prompt_tokens"),
        "completion_tokens": usage.get("completion_tokens"),
        "total_tokens": usage.get("total_tokens"),
    }
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))
    if not result["ok"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
