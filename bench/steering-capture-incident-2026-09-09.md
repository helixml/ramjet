# 2026-09-09 — steering capture window took down the node06 load balancer

## Timeline

- 2026-09-08 22:18Z — `escape-hardgate-capture-r2` window armed (guarded,
  plugin `qwen38-steering:0.4.0`, new canonical compose `b618c42…`). This was
  the first run of the capture campaign after the NVFP4 compose migration.
- The campaign's first mutation recreates `ds4-loadbalancer` single-homed to
  engine A (`RJ_UPSTREAM=http://qwen38flashnext-a:8000`). The campaign passed
  `RJ_UPSTREAM` but not the new canonical file's
  `RJ_ROUTE_KV_CAPACITY_TOKENS`, whose default is three values (`-,-,-`).
  One upstream + three capacity values ⇒ boot rejection:
  `invalid RJ_ROUTE_KV_CAPACITY_TOKENS: one value or one value per upstream`.
  The LB entered a restart loop (~503 restarts over ~8 h) and ports 8006/8007
  never listened.
- The campaign's `wait_lb` timed out (90 s), rollback ran, and the rollback
  recreate (three upstreams — capacity-legal) then hit a second, latent fault:
  `container image ID does not match adaptive config` — engine B had been
  re-imaged to the NVFP4 base (`5f1142f7`) on 09-07 while the on-box
  `adaptive-config.json` still pinned `0aea3024`. The check runs only at LB
  startup, so it had sat harmless under the running LB. Rollback could not
  restore a working LB; the campaign exited 3 (`rollback verification
  failed`) with the box down. Engine B itself had already been restored to
  baseline before the LB step — engines were never the problem.

## Recovery (operator, 2026-09-09 ~06:35Z)

Repinned only engine B's image in the on-box `adaptive-config.json` (backup
`adaptive-config.json.pre-nvfp4-b-20260909T063…Z`), recreated only
`ds4-loadbalancer` from the canonical file under the deployment lock.
Verified: restarts=0, `/health` 200, `ramjet_upstream_up` 1 for A+B, three
live completions 0.21–0.25 s, public endpoint answers 401 (not 502).

## Root causes

1. **Campaign compose() env drift at compose-schema migrations.** Every
   campaign recreates the LB with an explicit env set copied from the old
   compose schema. Any new multi-valued `RJ_*` env var must be added to
   `compose()` when a canonical compose gains it. `RJ_ROUTE_KV_CAPACITY_TOKENS`
   was missed in the NVFP4 port.
2. **`wait_lb` cannot see crash-looping.** It only curls `/health` until a
   90 s deadline; a container in `restart: unless-stopped` boot-rejection
   looks identical to slow startup.
3. **Adaptive-config image pins are a startup-only consistency gate.** They
   silently rot under a long-running LB and detonate on the first recreate —
   which is exactly what steering windows do.

## Fixes (this branch)

- All four campaigns (`qwen38_cyber_steering_capture`,
  `qwen38_cyber_steering_sweep`, `qwen38_steering_capture`,
  `qwen38_steering_eval`): `compose()` now passes
  `RJ_ROUTE_KV_CAPACITY_TOKENS` derived from the upstream list
  (`capacity_for`); `wait_lb` fails fast on `RestartCount != 0` and dumps the
  LB log tail instead of burning the deadline.
- `CLAUDE.md`: `BENCH_TOKEN` recipe reads `/etc/caddy/node06.env`
  (`BUNKER_API_KEY`); the old `grep 'Bearer …' /etc/caddy/Caddyfile` now
  silently yields an empty token (Caddyfile uses `{$BUNKER_API_KEY}`).

## Standing rules adopted

- Before arming any LB-recreating campaign after a compose/deploy change:
  (a) diff the canonical compose's `RJ_*` env set against `compose()`;
  (b) verify `adaptive-config.json` image IDs match the *running* engine
  containers (`docker inspect --format '{{.Image}}' qwen38flashnext-{a,b}` vs
  the config) — the match must be re-established in the same change that
  re-images an engine.
- Do not sync the repo's `deploy/qwen38_flash_next/adaptive-config.json`
  (both engines pinned NVFP4) to the box while engine A still runs
  `0aea3024`; that combination fails the same startup check. Converge A in
  the same change.
- Rollback for LB faults may be impossible while an adaptive-config fault is
  latent; keep the operator's manual path in mind (canonical recreate under
  the deployment lock, engines untouched).
