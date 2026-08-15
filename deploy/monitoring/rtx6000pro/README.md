# Grafana — MiniDynamo rtx6000pro

Canonical source for the serving dashboard of the 8× RTX PRO 6000 node
(currently node06). This directory is the source of truth; the infra
repository only carries the mirrored ConfigMap that Flux reconciles.

| | |
|---|---|
| dashboard | `minidynamo-rtx6000pro.json` |
| title | `MiniDynamo rtx6000pro` |
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

## Invariants the tests enforce

`bench/test_monitoring_dashboards.py` guards the parts that are easy to lose
when a dashboard is round-tripped through the UI:

- **Engine readiness must fold in `ds4proxy_idle_drain_state`.** A panel that
  reads `ds4proxy_upstream_up` alone renders an idle-drained engine as green
  READY instead of grey PAUSED, which reads as healthy capacity that is not
  actually serving. The exported preview had regressed exactly this.
- **The `Idle drain state` timeline must exist**, with the warm / draining /
  drained mappings.
- Both panels keep the `{{upstream}}` legend so they join on the same label.
- Canonical JSON formatting, unique panel ids, no grid overlaps, and no
  surviving `ds4-flash-serving` identity.

## Scope caveat

The name is per hardware profile, but the GPU and storage queries are still
pinned to `node="node06"` (and `mountpoint="/prod"`). A second RTX PRO 6000
box needs either a node template variable or its own copy — this dashboard
will not follow it automatically.
