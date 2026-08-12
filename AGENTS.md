# AGENTS.md — working on mini-dynamo

Guidance for coding agents (and humans) developing and testing mini-dynamo.
The production deployment is the DeepSeek-V4-Flash serving stack on **node06**;
full experiments require GPUs, so they run there, not locally.

## Local (no GPUs needed)

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked

# Keep the Go parity oracle healthy until the Rust cutover is promoted.
go test ./... && go vet ./...
gofmt -l .             # must be empty
```

The router is the interesting surface — `src/router.rs` contains the active
Rust tests and Go-generated fingerprint goldens; `pkg/router/router_test.go`
is the cutover oracle. Add a Rust test for every routing change and retain a
cross-language golden wherever Go/Rust parity matters.

## node06 — the test/production box

node06 is a Tailscale host running two vLLM+DSpark TP4 instances behind this
LB. Connect with the SSH alias (config already set up):

```bash
ssh node06            # root@100.89.187.17 via Tailscale
```

Layout on the box:

- `/home/luke/inference/dspark_0731/` — the whole serving stack (compose:
  `ds4-loadbalancer` + `dspark-0731` + `dspark-0731-b`), plus `.env`
  (`VLLM_API_KEY=<caddy bearer>`, mode 0600) and the bench scripts.
- Ports (loopback): LB API `:8006`, LB metrics `:8007`, engines `:8012`/`:8013`.
- The bearer token used by clients AND for engine probes:
  `grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile | head -1 | cut -d' ' -f2`
  (never hard-code it; it is not committed anywhere).

### Build + deploy a new LB image on node06

The interactive `gh` token lacks `write:packages`, so images are built on the
box until CI exists (ROADMAP). From your dev checkout:

```bash
# ship source, build the image tag on node06
tar czf /tmp/md.tgz --exclude=.git --exclude=target . && scp /tmp/md.tgz node06:/tmp/
ssh node06 'rm -rf /tmp/md && mkdir /tmp/md && tar xzf /tmp/md.tgz -C /tmp/md \
  && cd /tmp/md && docker build -t ghcr.io/helixml/ds4-loadbalancer:<tag> .'

# swap the LB (engines untouched; ~4s, LB-only)
ssh node06 'cd /home/luke/inference/dspark_0731 \
  && LB_IMAGE=ghcr.io/helixml/ds4-loadbalancer:<tag> docker compose up -d ds4-loadbalancer'

# verify
ssh node06 'docker logs ds4-loadbalancer --tail 1; curl -s :8007/metrics | grep ds4proxy_upstream_up'
```

Rollback is the same command with `LB_IMAGE=...:1.0.1`. The LB is stateless;
swapping it never touches the engines or their KV caches.

## Benchmarks (run on node06)

All benches are in `bench/` and mirrored to
`/home/luke/inference/dspark_0731/`. Export the token first:

```bash
ssh node06
export BENCH_TOKEN=$(grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile | head -1 | cut -d' ' -f2)
```

### Cache locality — `locality_bench.sh BASE APPS SESSIONS TURNS`

Simulates APPS apps (each a distinct ~18.5k-token system prompt) × SESSIONS
sessions × TURNS turns, sequential. Reports per-request `prompt cached wall`
and the total cache-hit %. **Use a fresh `SALT`** each run so prompts are
unseen (otherwise prior runs left them warm on both engines and you measure
nothing):

```bash
SALT=$(date +%s) ./locality_bench.sh http://127.0.0.1:8006 3 4 2
# cold prefills = rows with cached==0; count them:
#   awk 'NF==6 && $5==0' <output>
```

### Concurrent same-app load — `concurrent_sameapp.sh BASE N SALT TOK`

The test that separates the routers: N concurrent sessions sharing one system
prompt. Prints the upstream A/B split + aggregate tok/s. A load-blind router
sends all N to one instance; a good one spreads them.

```bash
./concurrent_sameapp.sh http://127.0.0.1:8006 12 $(date +%s) 256
```

### Aggregate throughput regression — `bench_serving.sh N TOK`

Standard N-way mixed sweep. Run after any change to confirm no throughput
regression. The current rc7 box code gate is in the 1,820–1,844 tok/s class at
c24/max256; the box is shared with live traffic, so expect run-to-run noise.

### Direct engine matrix — `engine_matrix.sh BASE MODEL LABEL`

For a rolling engine/image A/B, keep production single-homed on the other
engine and run the matched code+prose c1/c8/c16 matrix directly. The output is
six JSONL records with usage-token throughput, TTFT, and DSpark acceptance.
Point `METRICS_URL` at the same engine so production-wide counters do not
contaminate the speculative deltas:

```bash
METRICS_URL=http://127.0.0.1:8013/metrics \
  ./engine_matrix.sh http://127.0.0.1:8013 deepseek-v4-flash candidate \
  | tee /tmp/candidate-matrix.jsonl
```

Do not interpret draft acceptance alone when comparing capacity policies: a
pruned policy can raise the percentage by reducing its denominator. Compare
effective tokens/step and end-to-end throughput as well.

### A/B protocol

To compare two LB images cleanly: deploy image X, run a bench with a fresh
salt, capture; deploy image Y, run the SAME bench with ANOTHER fresh salt,
capture; compare. Never reuse a salt across the two — warm state leaks.
Read the router's own decisions with
`curl -s :8007/metrics | grep ds4proxy_route_decisions_total`.


### Decision journal + offline replay (rc5+)

Answer "would a different alpha/cap have routed better?" without touching
production: enable the journal, capture real traffic, replay counterfactuals.

```bash
# enable (LB env; stdout JSONL, privacy-bounded: no prompts/fingerprints/hosts)
ssh node06 'cd /home/luke/inference/dspark_0731 &&   DS4_ROUTE_JOURNAL=true LB_IMAGE=<current tag> docker compose up -d ds4-loadbalancer'

# capture + replay a policy sweep locally
ssh node06 'docker logs ds4-loadbalancer 2>&1' > /tmp/trace.log
python3 bench/route_replay.py /tmp/trace.log --alphas 1,2,4,8 --caps 8,16,32,64
```

Replay holds each observed cache/load snapshot fixed (the journal keeps no
prefix identity), so it compares single-decision policies — it cannot
simulate cache drift caused by earlier counterfactual choices. Findings and
caveats: EXPERIMENTS.md "rc5 privacy-bounded decision journal and replay".

## Full experiment checklist

The repeatable end-to-end loop (each past run is written up in
EXPERIMENTS.md — add yours there too):

1. **Local**: `go test ./... && go vet ./...`; add/extend a router test for
   any routing change.
2. **Build + deploy candidate** on node06 (section above) with a fresh
   `<tag>`; confirm `ds4proxy_upstream_up` shows both engines and the boot
   log line has the config you meant to ship.
3. **Bench matrix** (fresh SALT per run, per the A/B protocol):
   locality (`locality_bench.sh`), concurrent same-app
   (`concurrent_sameapp.sh`), aggregate regression (`bench_serving.sh 16 512`).
4. **Route telemetry**: `curl -s :8007/metrics | grep -E
   "route_decisions|route_overlap|upstream_inflight|upstream_load_units"` —
   confirm the decision mix moved the way the change predicts.
5. **Helix end-to-end**: one real session via `POST $HELIX_URL/api/v1/
   sessions/chat` against the org test app (ids + creds: infra repo,
   `node06/inference/dspark_0731/README.md`). This catches harness-shim
   regressions that synthetic benches miss.
6. **Record**: append the run to EXPERIMENTS.md (config, numbers, verdict),
   update RESULTS.md if it changes a headline, and either promote the tag in
   the infra compose (`LB_IMAGE` default) or note why not.
7. **Watch after promote**: Grafana `ds4-flash-serving` for 10-15 min
   (5xx, TTFT p95, upstream split) — rollback is one `LB_IMAGE` flip.

## Guardrails

- The LB sits in the **production inference path** (Helix agent fleet). Bench
  traffic shares the box with real users — keep concurrency modest and
  prefer short `max_tokens`. Never stop the engines to test the LB.
- Secrets (Caddy bearer, Helix API key) are fetched on-box, never committed.
- Metric names keep the `ds4proxy_` prefix for dashboard continuity — do not
  rename without updating `clusters/bunker/monitoring/` in the infra repo.
- Verify end-to-end through Helix after a deploy, not just the LB:
  a chat via `POST $HELIX_URL/api/v1/sessions/chat` with the org's app id
  (connection details in the infra repo's `node06/inference/dspark_0731/
  README.md`, not here).
