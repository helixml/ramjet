#!/usr/bin/env python3
"""Open-loop simulation of a team of developers driving coding agents.

What makes routing matter in real agent traffic is layered prefix reuse,
contexts that grow to tens of thousands of tokens, and time between turns.
This generator models all three:

* Prefix layers. Every developer uses one of a few agent harnesses (system
  prompt plus tool schemas, shared by everyone on that harness), works in one
  of a few repositories (project instructions and file tree, shared by
  everyone in that repository), and then accumulates a private session
  history.
* Sessions. A task opens with a user request, then alternates model turns
  (mostly short tool calls, sometimes long code-writing turns) with tool
  results of heavy-tailed size. When the context passes --compact-at tokens
  the agent compacts: history is replaced with a summary, which starts a new
  branch off the repository prefix. A task ends after a geometric number of
  turns and the developer starts another after a pause.
* Time. Tool execution takes seconds, humans occasionally pause for tens of
  seconds, and some turns fan out parallel sub-agents that share the harness.

Prompts are salted synthetic filler and nothing here carries user data. The
summary excludes a warm-up window. Output: one JSON summary on stdout and,
optionally, one content-free JSON line per request.
"""

import argparse
import http.client
import json
import math
import os
import random
import sys
import threading
import time
import urllib.parse

WORDS = (
    "fn let mut impl struct enum match return self crate pub use mod trait type "
    "async await result error option some none ok err vec string map iter into "
    "config router proxy upstream request response header body stream token cache "
    "test assert build deploy commit branch merge diff patch file path line column "
    "index value key field record table query schema handler client server socket "
    "thread lock channel timeout retry backoff metric counter gauge label span trace"
).split()


class Filler:
    """Deterministic synthetic text sized in model tokens.

    `words_per_token` is calibrated once against the endpoint so requested
    sizes are in real prompt tokens rather than words.
    """

    def __init__(self, words_per_token=1.0):
        self.words_per_token = words_per_token

    def text(self, rng, tokens):
        words = max(1, int(tokens * self.words_per_token))
        out = []
        for i in range(words):
            out.append(rng.choice(WORDS))
            if i % 13 == 12:
                out.append(f"{rng.randrange(10**5)};\n")
        return " ".join(out)


def lognormal(rng, median, sigma, lo, hi):
    return max(lo, min(hi, rng.lognormvariate(math.log(median), sigma)))


def percentile(values, q):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, round(q / 100 * (len(ordered) - 1))))]


class Client:
    def __init__(self, base, token, timeout):
        parsed = urllib.parse.urlsplit(base)
        self.host, self.port = parsed.hostname, parsed.port or 80
        self.prefix = parsed.path.rstrip("/")
        self.token = token
        self.timeout = timeout

    def chat(self, body):
        conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
        headers = {"Content-Type": "application/json"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        start = time.perf_counter()
        try:
            conn.request("POST", f"{self.prefix}/v1/chat/completions", json.dumps(body), headers)
            response = conn.getresponse()
            upstream = response.getheader("x-ramjet-upstream")
            if response.status != 200:
                raise RuntimeError(f"HTTP {response.status}: {response.read(300).decode(errors='replace')}")
            first = None
            content = []
            usage = {}
            for raw in response:
                line = raw.strip()
                if not line.startswith(b"data:"):
                    continue
                payload = line[5:].strip()
                if payload == b"[DONE]":
                    break
                event = json.loads(payload)
                if event.get("usage"):
                    usage = event["usage"]
                for choice in event.get("choices") or []:
                    delta = choice.get("delta") or {}
                    if first is None and (delta.get("content") or delta.get("reasoning_content")):
                        first = time.perf_counter()
                    if delta.get("content"):
                        content.append(delta["content"])
            end = time.perf_counter()
            return {
                "start": start,
                "ttft": (first or end) - start,
                "e2e": end - start,
                "usage": usage,
                "content": "".join(content),
                "upstream": upstream,
            }
        finally:
            conn.close()


class Swarm:
    def __init__(self, args):
        self.args = args
        self.client = Client(args.base, os.environ.get("BENCH_TOKEN"), args.timeout)
        self.filler = Filler()
        self.lock = threading.Lock()
        self.records = []
        self.tasks = []
        self.out = open(args.requests_jsonl, "a") if args.requests_jsonl else None
        self.started = None
        self.stop_at = None

    # ---- prompt layers -------------------------------------------------
    def build_layers(self):
        a = self.args
        self.harnesses = []
        for h in range(a.harnesses):
            rng = random.Random(f"{a.salt}:harness:{h}")
            tokens = int(a.harness_tokens * (0.7 + 0.6 * h / max(1, a.harnesses - 1)))
            self.harnesses.append(
                f"You are coding agent harness {h} ({a.salt}). Tools and rules follow.\n"
                + self.filler.text(rng, tokens)
            )
        self.repos = []
        for r in range(a.repos):
            rng = random.Random(f"{a.seed}:repo:{r}")
            tokens = int(lognormal(rng, a.repo_tokens, 0.5, a.repo_tokens / 4, a.repo_tokens * 3))
            text_rng = random.Random(f"{a.salt}:repo:{r}")
            self.repos.append(f"Repository {r} instructions and tree:\n" + self.filler.text(text_rng, tokens))

    def calibrate(self):
        """Measure prompt tokens per filler word through the endpoint."""
        rng = random.Random(f"{self.args.salt}:calibrate")
        words = 4000
        text = " ".join(rng.choice(WORDS) if i % 13 else f"{rng.randrange(10**5)};\n" for i in range(words))
        body = {
            "model": self.args.model,
            "messages": [{"role": "user", "content": text}],
            "max_tokens": 1,
            "stream": True,
            "stream_options": {"include_usage": True},
        }
        result = self.client.chat(body)
        tokens = result["usage"].get("prompt_tokens") or words
        self.filler.words_per_token = words / tokens
        return tokens / words

    # ---- request plumbing ----------------------------------------------
    def emit(self, record):
        with self.lock:
            self.records.append(record)
            if self.out:
                self.out.write(json.dumps(dict(record, label=self.args.label)) + "\n")
                self.out.flush()
            done = len(self.records)
        if done % 100 == 0:
            print(
                f"progress requests={done} elapsed={time.perf_counter() - self.started:.0f}s",
                file=sys.stderr,
                flush=True,
            )

    def turn(self, messages, max_tokens, meta):
        body = {
            "model": self.args.model,
            "messages": messages,
            "max_tokens": max_tokens,
            "min_tokens": max_tokens,
            "ignore_eos": True,
            "temperature": 0.0,
            "stream": True,
            "stream_options": {"include_usage": True},
            "reasoning_effort": self.args.reasoning_effort,
        }
        record = dict(meta)
        record["t"] = round(time.perf_counter() - self.started, 3)
        try:
            result = self.client.chat(body)
        except Exception as error:  # noqa: BLE001 - recorded, never fatal
            record.update(ok=False, error=type(error).__name__, detail=str(error)[:160])
            self.emit(record)
            return None
        usage = result["usage"]
        prompt = usage.get("prompt_tokens") or 0
        cached = (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
        completion = usage.get("completion_tokens") or 0
        record.update(
            ok=True,
            upstream=result["upstream"],
            ttft=round(result["ttft"], 4),
            e2e=round(result["e2e"], 4),
            prompt_tokens=prompt,
            cached_tokens=cached,
            completion_tokens=completion,
            tpot=round((result["e2e"] - result["ttft"]) / (completion - 1), 5) if completion > 1 else None,
        )
        self.emit(record)
        return result["content"] or "(tool call)", prompt

    def sleep_until(self, seconds):
        remaining = self.stop_at - time.perf_counter()
        time.sleep(max(0.0, min(seconds, remaining)))
        return time.perf_counter() < self.stop_at

    # ---- workload --------------------------------------------------------
    def output_tokens(self, rng):
        if rng.random() < self.args.long_turn_share:
            return int(lognormal(rng, 900, 0.4, 400, 2000))
        return int(lognormal(rng, 120, 0.5, 32, 400))

    def tool_tokens(self, rng):
        return int(lognormal(rng, self.args.tool_median_tokens, 0.9, 80, 16000))

    def subagent(self, dev, task, harness, key, parent_turn):
        rng = random.Random(f"{self.args.seed}:{key}")
        trng = random.Random(f"{self.args.salt}:{key}")
        messages = [
            {"role": "system", "content": self.harnesses[harness]},
            {"role": "user", "content": "Sub-agent task: " + self.filler.text(trng, 400)},
        ]
        for step in range(rng.randint(2, 5)):
            if time.perf_counter() >= self.stop_at:
                return
            reply = self.turn(
                messages,
                self.output_tokens(rng),
                {"kind": "subagent", "dev": dev, "task": task, "turn": parent_turn, "step": step},
            )
            if reply is None:
                return
            messages.append({"role": "assistant", "content": reply[0]})
            messages.append({"role": "user", "content": "Tool result: " + self.filler.text(trng, self.tool_tokens(rng) // 2)})
            if not self.sleep_until(lognormal(rng, 1.5, 0.6, 0.2, 8)):
                return

    def developer(self, dev):
        a = self.args
        # Structure (sizes, timing, choices) follows --seed so every run of
        # a comparison sees the same workload; text follows --salt so no run
        # inherits another's cache.
        rng = random.Random(f"{a.seed}:dev:{dev}")
        trng = random.Random(f"{a.salt}:dev:{dev}")
        # Harness and repository popularity are skewed, as in a real team.
        harness = min(int(rng.paretovariate(1.6)) - 1, a.harnesses - 1)
        repo = min(int(rng.paretovariate(1.2)) - 1, a.repos - 1)
        if not self.sleep_until(rng.uniform(0, a.ramp)):
            return
        task = 0
        while time.perf_counter() < self.stop_at:
            task_started = time.perf_counter()
            base = [
                {"role": "system", "content": self.harnesses[harness] + "\n\n" + self.repos[repo]},
            ]
            messages = base + [
                {"role": "user", "content": f"Task {dev}-{task}: " + self.filler.text(trng, int(lognormal(rng, 600, 0.7, 50, 4000)))}
            ]
            turns = min(a.max_turns, 1 + int(rng.expovariate(1 / a.mean_turns)))
            compactions = 0
            completed = True
            for turn in range(turns):
                if time.perf_counter() >= self.stop_at:
                    completed = False
                    break
                reply = self.turn(
                    messages,
                    self.output_tokens(rng),
                    {"kind": "agent", "dev": dev, "task": task, "turn": turn, "harness": harness, "repo": repo, "compactions": compactions},
                )
                if reply is None:
                    completed = False
                    break
                reply, context = reply
                if rng.random() < a.subagent_share:
                    for k in range(rng.randint(1, a.max_subagents)):
                        threading.Thread(
                            target=self.subagent,
                            args=(dev, task, harness, f"sub:{dev}:{task}:{turn}:{k}", turn),
                            daemon=True,
                        ).start()
                messages.append({"role": "assistant", "content": reply})
                messages.append({"role": "user", "content": "Tool result: " + self.filler.text(trng, self.tool_tokens(rng))})
                if context >= a.compact_at:
                    # Compaction: keep harness+repo, replace history with a summary.
                    compactions += 1
                    messages = base + [
                        {"role": "user", "content": f"Summary of task {dev}-{task} so far: " + self.filler.text(trng, 2500)}
                    ]
                pause = lognormal(rng, a.tool_seconds, 0.8, 0.3, 30)
                if rng.random() < a.human_pause_share:
                    pause += lognormal(rng, 45, 0.7, 10, 240)
                if not self.sleep_until(pause):
                    completed = False
                    break
            with self.lock:
                self.tasks.append(
                    {
                        "dev": dev,
                        "task": task,
                        "turns": turn + 1,
                        "completed": completed,
                        "start": task_started - self.started,
                        "seconds": time.perf_counter() - task_started,
                    }
                )
            task += 1
            if not self.sleep_until(lognormal(rng, a.between_tasks_seconds, 0.6, 2, 300)):
                return

    def run(self):
        self.started = time.perf_counter()
        ratio = self.calibrate()
        self.build_layers()
        self.started = time.perf_counter()
        self.stop_at = self.started + self.args.duration
        threads = [threading.Thread(target=self.developer, args=(dev,), daemon=True) for dev in range(self.args.developers)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        time.sleep(1.0)  # let straggling sub-agents record
        wall = time.perf_counter() - self.started
        if self.out:
            self.out.close()
        return self.summary(wall, ratio)

    # ---- reporting --------------------------------------------------------
    def summary(self, wall, tokens_per_word):
        a = self.args
        with self.lock:
            records = list(self.records)
            tasks = list(self.tasks)
        window = [r for r in records if r["t"] >= a.warmup]
        ok = [r for r in window if r.get("ok")]
        seconds = max(1e-9, wall - a.warmup)
        prompt = sum(r["prompt_tokens"] for r in ok)
        cached = sum(r["cached_tokens"] for r in ok)
        completion = sum(r["completion_tokens"] for r in ok)

        def pct(values):
            return {f"p{q}": (round(percentile(values, q), 3) if values else None) for q in (50, 90, 99)}

        buckets = {}
        for lo, hi, name in ((0, 32_000, "<32k"), (32_000, 64_000, "32-64k"), (64_000, 128_000, "64-128k"), (128_000, 10**9, ">=128k")):
            rs = [r for r in ok if lo <= r["prompt_tokens"] < hi]
            if rs:
                p = sum(r["prompt_tokens"] for r in rs)
                buckets[name] = {
                    "requests": len(rs),
                    "ttft": pct([r["ttft"] for r in rs]),
                    "cache_ratio": round(sum(r["cached_tokens"] for r in rs) / p, 4) if p else None,
                }
        per_upstream = {}
        for r in ok:
            slot = per_upstream.setdefault(r.get("upstream") or "direct", {"requests": 0, "prompt": 0, "cached": 0})
            slot["requests"] += 1
            slot["prompt"] += r["prompt_tokens"]
            slot["cached"] += r["cached_tokens"]
        for slot in per_upstream.values():
            slot["cache_ratio"] = round(slot["cached"] / slot["prompt"], 4) if slot["prompt"] else None
        chains = {}
        for r in sorted((r for r in ok if r["kind"] == "agent"), key=lambda r: (r["dev"], r["task"], r["turn"])):
            chains.setdefault((r["dev"], r["task"]), []).append(r.get("upstream"))
        stays = sum(p == c for ups in chains.values() for p, c in zip(ups, ups[1:]))
        moves = sum(p != c for ups in chains.values() for p, c in zip(ups, ups[1:]))
        agent = [r for r in ok if r["kind"] == "agent"]
        done_tasks = [t for t in tasks if t["completed"] and t["start"] >= a.warmup]
        slo = {f"ttft_le_{s}s": round(sum(r["ttft"] <= s for r in agent) / len(agent), 4) if agent else None for s in (2, 5, 10)}
        return {
            "label": a.label,
            "developers": a.developers,
            "duration_s": a.duration,
            "warmup_s": a.warmup,
            "salt": a.salt,
            "seed": a.seed,
            "tokens_per_word": round(tokens_per_word, 3),
            "requests_ok": len(ok),
            "requests_failed": sum(1 for r in window if not r.get("ok")),
            "agent_turns": len(agent),
            "subagent_turns": len(ok) - len(agent),
            "turns_per_min": round(len(ok) / seconds * 60, 1),
            "output_tok_s": round(completion / seconds, 1),
            "prompt_tok_s": round(prompt / seconds, 1),
            "uncached_prompt_tok_s": round((prompt - cached) / seconds, 1),
            "cache_ratio": round(cached / prompt, 4) if prompt else None,
            "mean_prompt_tokens": round(prompt / len(ok)) if ok else None,
            "max_prompt_tokens": max((r["prompt_tokens"] for r in ok), default=None),
            "ttft": pct([r["ttft"] for r in agent]),
            "ttft_subagent": pct([r["ttft"] for r in ok if r["kind"] == "subagent"]),
            "tpot": pct([r["tpot"] for r in ok if r.get("tpot")]),
            "e2e": pct([r["e2e"] for r in agent]),
            "slo_goodput": slo,
            "by_context": buckets,
            "session_stickiness": round(stays / (stays + moves), 4) if stays + moves else None,
            "tasks_completed": len(done_tasks),
            "task_seconds": pct([t["seconds"] for t in done_tasks]),
            "per_upstream": per_upstream,
        }


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("base")
    p.add_argument("model")
    p.add_argument("--label", default="")
    p.add_argument("--developers", type=int, default=64)
    p.add_argument("--duration", type=float, default=900)
    p.add_argument("--warmup", type=float, default=180)
    p.add_argument("--ramp", type=float, default=60)
    p.add_argument("--harnesses", type=int, default=3)
    p.add_argument("--harness-tokens", type=int, default=18000)
    p.add_argument("--repos", type=int, default=6)
    p.add_argument("--repo-tokens", type=int, default=6000)
    p.add_argument("--mean-turns", type=float, default=30)
    p.add_argument("--max-turns", type=int, default=120)
    p.add_argument("--tool-median-tokens", type=int, default=1500)
    p.add_argument("--long-turn-share", type=float, default=0.15)
    p.add_argument("--compact-at", type=int, default=110_000)
    p.add_argument("--tool-seconds", type=float, default=2.0)
    p.add_argument("--human-pause-share", type=float, default=0.05)
    p.add_argument("--between-tasks-seconds", type=float, default=20)
    p.add_argument("--subagent-share", type=float, default=0.04)
    p.add_argument("--max-subagents", type=int, default=3)
    p.add_argument("--reasoning-effort", default="low")
    p.add_argument("--timeout", type=float, default=900)
    p.add_argument("--salt", default=str(time.time_ns()), help="prompt text; fresh per run")
    p.add_argument("--seed", default="swarm-v1", help="workload structure; fixed across a comparison")
    p.add_argument("--requests-jsonl")
    args = p.parse_args()
    print(json.dumps(Swarm(args).run()))


if __name__ == "__main__":
    main()
