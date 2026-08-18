# Grafana — Ramjet rtx6000pro

Canonical source for the serving dashboard of the 8× RTX PRO 6000 node
(currently node06). This directory is the source of truth; the infra
repository only carries the mirrored ConfigMap that Flux reconciles.

| | |
|---|---|
| dashboard | `minidynamo-rtx6000pro.json` |
| title | `Ramjet rtx6000pro` |
| uid | `minidynamo-rtx6000pro` |
| ConfigMap | `bunker-dashboards` (key `minidynamo-rtx6000pro.json`) |
| infra path | `clusters/bunker/monitoring/grafana-dashboards.yaml` |

It replaces `DeepSeek V4 Flash Serving (node06)` (uid `ds4-flash-serving`).
The layout came from the hand-tuned Grafana "Layout Preview" copy: header
stats, the six GPU gauges promoted above the fold, then the LB/engine time
series, then the per-GPU diagnostics. Both retired dashboards should be
deleted from Grafana; the sidecar removes `ds4-flash-serving` on its own
because the sync drops that ConfigMap key, but the ad-hoc
`ds4-flash-serving-layout-preview` was never provisioned and must be deleted
by hand.

## Editing

Edit the JSON here, never the infra copy, then mirror it:

```bash
python3 deploy/monitoring/rtx6000pro/sync-dashboards.py --check ../infra
python3 deploy/monitoring/rtx6000pro/sync-dashboards.py ../infra
python3 -m unittest bench.test_monitoring_dashboards
```

The sync rewrites only the keys this directory owns and leaves every other
dashboard in that ConfigMap byte-identical. `--check` exits non-zero when the
mirror is stale and changes nothing.

If you tweak a dashboard in the Grafana UI first, export it, drop it in here,
and let the tests re-canonicalize the formatting — do not paste an export
straight into the infra ConfigMap. A UI export also carries an `"id"` field
and a bumped `"version"`; strip the `id`, because it collides with whatever
Grafana already stores for that uid.

## Idle drain

The idle-drain policy parks one engine during a quiet window to save idle
power. It is **off** in the canonical deployment (`RJ_IDLE_DRAIN_MODE` is
unset), and it exports nothing at all while off — so every panel below reads
`idle-drain policy is off` rather than going blank.

Two places show it:

- **`Engine readiness`**, always visible at the top. A parked engine reads grey
  PAUSED, not green READY. Health multiplies the drain term rather than adding
  to it, so a *stopped* engine still reads DOWN even while it is drained; the
  trailing `or (ramjet_upstream_up * 0)` keeps the tile working when the
  policy is off and exports no drain series at all.
- **The collapsed `Idle drain (idle power parking)` row** at the bottom, which
  carries the four diagnostics. It stays folded because the policy is off; open
  it when qualifying `observe` mode.

| panel | metric | reads |
|---|---|---|
| Idle drain state | `ramjet_idle_drain_state` | warm / draining / drained |
| Stop intent | `..._desired_running`, `..._safe_to_stop` | the converger's two inputs |
| Fleet idle window | `..._fleet_idle` | serving / idle |
| Drain transitions (per hour) | `rate(..._transitions_total)` | flapping detector |

The LB never stops a container itself — it publishes intent and a separately
privileged actor converges on it, stopping an engine only when desired running
is `no` **and** safe to stop is `yes`. Neither value alone is an instruction,
which is why they share one panel. The transitions rate is the qualification
signal: a badly tuned idle threshold shows up as draining/warm churn, not as an
error, and AGENTS.md requires a clean interval there before dual-pair observe.

## Invariants the tests enforce

`bench/test_monitoring_dashboards.py` guards the parts that are easy to lose
when a dashboard is round-tripped through the UI:

- **Engine readiness must fold in `ramjet_idle_drain_state`**, keep the
  policy-off fallback, and keep the DOWN / READY / PAUSED mappings. A panel
  that reads `ramjet_upstream_up` alone renders an idle-drained engine as
  green READY — healthy-looking capacity that is not serving. The exported
  preview had regressed exactly this.
- **All five `ramjet_idle_drain_*` metrics stay on the dashboard**, with the
  transitions counter shown as a rate rather than a raw total.
- **Every `ramjet_*` query resolves against `src/metrics.rs`**, including
  label sets. Renaming a metric in Rust otherwise leaves valid PromQL that
  silently renders "No data".
- Panels that depend on the policy carry a `noValue` explaining it may be off.
- Canonical JSON formatting, unique panel ids (rows share that id space), no
  grid overlaps within the top level or within a row, and no surviving
  `ds4-flash-serving` identity.

## Scope caveat

The name is per hardware profile, but the GPU and storage queries are still
pinned to `node="node06"` (and `mountpoint="/prod"`). A second RTX PRO 6000
box needs either a node template variable or its own copy — this dashboard
will not follow it automatically.
