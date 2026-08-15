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
mini-dynamo LB               samples every MD_MACHINEVIEW_INTERVAL_MS:
  src/machineview.rs           - its own Prometheus registry (ds4proxy_*)
        │                      - each upstream's /metrics (vllm:*)
        │                      - the agent (host + GPUs + energy)
        │                    stores a bounded in-memory ring (optionally
        │                    persisted), serves JSON + this UI on :9090
        ▼
/ui/  +  /api/machineview/{summary,series}
```

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
| `MD_MACHINEVIEW_MODE` | `on` | `off` disables sampling, API, and UI |
| `MD_MACHINEVIEW_INTERVAL_MS` | `5000` | sampling cadence (1000–60000) |
| `MD_MACHINEVIEW_RETENTION_SECONDS` | `86400` | ring retention (60–604800) |
| `MD_MACHINEVIEW_AGENT_URL` | unset | host agent `/sample` URL; without it there is no host/GPU/energy telemetry |
| `MD_MACHINEVIEW_STATE_PATH` | unset | JSON snapshot path; restores history across LB restarts |
| `MD_MACHINEVIEW_UI_DIR` | `/ui` if present | static bundle directory |

Engine (`vllm:*`) scraping needs no configuration — it reuses the configured
`MD_UPSTREAM` list. Without the agent the serving charts still work; the
machine/GPU sections show their empty states.

## Host agent

Stdlib-only, loopback-only by default, read-only:

```bash
python3 bench/machineview_agent.py --port 8016 --mounts /,/home/luke
```

On a Docker-bridge deployment the LB reaches it via the gateway address,
e.g. `MD_MACHINEVIEW_AGENT_URL=http://172.17.0.1:8016/sample`. It reports
CPU busy share, load, memory/swap, per-mount usage, whole-disk I/O rates,
physical-interface network rates, RAPL package watts, and one row per GPU
from `nvidia-smi`. Rates need two scrapes, so the first sample returns nulls.
