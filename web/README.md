# Machine view — the built-in dashboard

A single-page dashboard for the whole serving box: token throughput, TTFT,
sessions, KV-cache and prefix-cache state per engine, plus host CPU, memory,
storage, network, disk I/O, per-GPU utilization/memory/power/thermals, and
cumulative energy. Metrics are sampled and stored locally by the load
balancer (`src/machineview.rs`) — no Prometheus or Grafana required.

Stack: Vite + React + TypeScript, Tailwind v4, shadcn conventions
(`components.json`, `@/` aliases, CSS-variable theming), Recharts. The color
system is a validated colorblind-safe palette with independently stepped
light and dark modes; every chart has a legend (≥2 series), crosshair
tooltips, and a table-view twin behind the toggle in each card header.

## How it fits together

```
bench/machineview_agent.py   host agent (loopback): /proc, statvfs, RAPL, nvidia-smi
        │  GET /sample (JSON)
        ▼
ramjet LB               samples every RJ_MACHINEVIEW_INTERVAL_MS:
  src/machineview.rs           - its own Prometheus registry (ramjet_*)
        │                      - each upstream's /metrics (vllm:*)
        │                      - the agent (host + GPUs + energy)
        │                    stores a bounded in-memory ring plus a long
        │                    hourly token history (optionally persisted),
        │                    serves JSON + this UI on :9090
        ▼
/ui/  +  /api/machineview/{summary,series,tokens}
      +  /api/machineview/stream   (WebSocket, RJ_MACHINEVIEW_STREAM_INTERVAL_MS)
```

Three cadences, deliberately. `RJ_MACHINEVIEW_INTERVAL_MS` (5 s) is what
costs network: it scrapes every engine's `/metrics` and the host agent, and
it is what the ring stores. `RJ_MACHINEVIEW_STREAM_INTERVAL_MS` (1 s) reads
only the proxy's own in-process registry, so serving throughput, TTFT, TPOT,
in-flight and per-upstream request rate stream at 1 Hz without adding a
single request to an engine. Full samples are pushed onto the same socket as
they land. Nothing is published while no client is connected, and the rate
tracker resets when the last one leaves, so an unwatched dashboard costs
nothing.

The socket is an accelerator, never a requirement. The UI keeps polling
underneath and reconnects with backoff, so an LB without the route, a proxy
that will not upgrade, or a dropped connection degrades to the 5 s dashboard
rather than an empty one. Engine- and host-derived charts (KV cache, running
and waiting per engine, GPU, disk, power) stay at the sampling interval —
their data is scraped, not in-process, and a live frame carries no host or
engine fields to interpolate from. At most 8 clients stream at once; a
client too slow for the interval is dropped rather than served a backlog.

Two stores, two time scales. The ring answers "what is the box doing now"
at seconds of resolution and is bounded by `RJ_MACHINEVIEW_RETENTION_SECONDS`
(a day by default, a week at most). The token history answers "when does this
box get used" from the same `ramjet_*` counters at one bucket an hour, so a
month of it costs 720 small records — that is what the Overview's two
token heatmaps read.

## Development

```bash
cd web
npm install

# Live node06 data with zero setup (on the VPN): web/.env points the dev
# proxy at node06's LB, so this alone shows the real box with HMR.
npm run dev                # open http://localhost:5173/ui/

# Pure UI work, no backend at all — deterministic synthetic data:
npm run dev                # then open http://localhost:5173/ui/?mock=1

# Another box or a locally running LB: a shell UI_PROXY_TARGET beats web/.env.
UI_PROXY_TARGET=http://127.0.0.1:9090 npm run dev

npm run build              # tsc + vite; output lands in web/dist
```

The dev server proxies both `/api` and `/metrics` to the same target, so the
Prometheus button works in dev too.

The production bundle is built in the Dockerfile's `ui` stage and copied to
`/ui` in the LB image, where the Rust side serves it on the metrics listener
(`:9090` in-container, `:8007` on node06). With the metrics port published
on the box's tailnet address, no dev server is needed at all — anyone on the
VPN opens http://100.89.187.17:8007/ui/ directly.

## Runtime configuration (LB side)

| Variable | Default | Meaning |
|---|---|---|
| `RJ_MACHINEVIEW_MODE` | `on` | `off` disables sampling, API, and UI |
| `RJ_MACHINEVIEW_INTERVAL_MS` | `5000` | sampling cadence (1000–60000) |
| `RJ_MACHINEVIEW_RETENTION_SECONDS` | `86400` | ring retention (60–604800) |
| `RJ_MACHINEVIEW_TOKEN_HISTORY_DAYS` | `30` | hourly token-history retention (1–400) |
| `RJ_MACHINEVIEW_STREAM_INTERVAL_MS` | `1000` | live WebSocket cadence for registry-derived serving metrics (200–10000) |
| `RJ_MACHINEVIEW_AGENT_URL` | unset | host agent `/sample` URL; without it there is no host/GPU/energy telemetry |
| `RJ_MACHINEVIEW_STATE_PATH` | unset | JSON snapshot path; restores both the ring and the token history across LB restarts |
| `RJ_MACHINEVIEW_UI_DIR` | `/ui` if present | static bundle directory |
| `RJ_UI_AUTH_TOKEN` | unset | Dedicated 32–256-byte dashboard/control token. Protects machine-view and adaptive APIs with a signed 30-day HttpOnly session cookie. |

Engine (`vllm:*`) scraping needs no configuration — it reuses the configured
`RJ_UPSTREAM` list. Without the agent the serving charts still work; the
machine/GPU sections show their empty states.

## Host agent

Stdlib-only, loopback-only by default, read-only:

```bash
python3 bench/machineview_agent.py --port 8016 --mounts /,/home/luke
```

On a Docker-bridge deployment the LB reaches it via the gateway address,
e.g. `RJ_MACHINEVIEW_AGENT_URL=http://172.17.0.1:8016/sample`. It reports
CPU busy share, load, memory/swap, per-mount usage, whole-disk I/O rates,
physical-interface network rates, RAPL package watts, and one row per GPU
from `nvidia-smi`. Rates need two scrapes, so the first sample returns nulls.

## Where the cache-hit number comes from

`serving.cache_hit_pct` has two possible sources, and the sample says which
one it used in `serving.cache_hit_source`:

- `response_usage` — the LB's own `ramjet_cached_prompt_tokens_total` over
  `ramjet_prompt_tokens_total`. Authoritative, because it is measured on the
  responses this proxy actually served.
- `engine_prefix_cache` — summed `vllm:prefix_cache_hits_total` over summed
  `vllm:prefix_cache_queries_total` across the scraped engines.

The fallback exists because an engine that never populates
`prompt_tokens_details.cached_tokens` leaves every cache outcome `unknown` and
the LB ratio permanently absent. Qwen3.8 on node06 is exactly that case: the
proxy could not see a hit rate while the engines were reporting ~90%. The
fallback only ever fills an absent value, so a fleet that does report
`cached_tokens` keeps the authoritative figure.

Read the engine figure as strictly weaker. It counts every query the engines
saw, including any traffic this LB did not route, and it measures vLLM's block
lookups rather than the tokens billed to a response. Rates are summed before
dividing so the ratio stays token-weighted; averaging four per-engine
percentages would let a nearly idle engine count as much as one carrying the
whole fleet. A `queries` rate of zero yields absence, not `0%` — a quiet
interval is not a cold cache.

### How it is plotted

The card and the tile both show a **trailing rolling average**, not the
per-sample ratio (`src/lib/rolling.ts`). Sampled over five seconds of bursty
traffic, an instantaneous hit rate is 0% or 100% and rarely anything between,
and an idle interval has no ratio at all — plotted raw that is a comb of
vertical hairlines rather than a rate, because a filled area draws each
isolated point as a spike from zero. The window is 5% of the selected range
(clamped to 1–10 minutes), so it scales with the pixels available and spans
the gaps between bursts instead of breaking the line at each one.

Samples are weighted by the prompt-token rate they were measured over, so a
busy second counts for more than a quiet one — the same reason the engine
ratio sums rates before dividing. They are also weighted by their own
duration, so the 1 Hz live tail spliced onto the 5 s history does not swamp
the older, sparser half of the window. Absent samples stay absent: a window
containing none of them is a gap, never `0%`.

The line follows `serving.cache_hit_pct` whichever layer produced it, and
falls back to the unweighted mean of per-engine `prefix_hit_pct` only when no
weighted figure exists at all; the card's subtitle names the source and the
window. Because the fallback ratio is filled in by the 5 s sampler and not by
the 1 Hz stream, an engine-reported fleet plots the polled series — following
the live tail there would end the line minutes short of now.

## The two token heatmaps

They sit in the Overview beside the GPU row, compact by design: `Tokens by
day` is one column per date and one row per three-hour band, `Tokens by hour`
is a weekday × hour-of-day punchcard summed over the window. Both plot prompt
+ generated tokens as dots, and both are independent of the range picker,
which drives every other card. The full-precision numbers are behind the
table toggle in each card header.

Four things about them are deliberate:

- **Buckets are stored in UTC and displayed in the viewer's local time.**
  "When does this box get used" is a wall-clock question, so the grouping
  happens in the browser; the stored series stays unambiguous.
- **Two encodings of the same number.** Dot *area* is proportional to the
  value, which is the honest encoding and makes one enormous hour look
  enormous; *color* steps by quantile of the active cells, because volume is
  skewed enough that area alone leaves the whole quiet half as indistinct
  specks. The exact numbers are one hover away and all of them are in the
  table view.
- **Idle and unknown are different marks.** A recorded hour with no traffic
  is a solid speck; an hour with no bucket at all — the LB was down, or the
  history simply starts later — is a hollow ring. They must not look the same.
- **Restarts lose the in-flight interval, not the history.** The counters are
  cumulative, so a restart re-baselines rather than logging a negative delta,
  and with `RJ_MACHINEVIEW_STATE_PATH` set the accumulated buckets survive.

The tooltip's `requests` line is `ramjet_requests_total` across every
endpoint and status code, so a quiet hour can show requests with no tokens.
`cached` is the LB's own `ramjet_cached_prompt_tokens_total`, which stays
at zero against engines that do not return `prompt_tokens_details` — Qwen3.8
is one of them, so on node06 that line is structurally zero, not measured.
