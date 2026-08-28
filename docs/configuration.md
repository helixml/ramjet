# Configuration

ramjet is configured at startup through environment variables. The normal
serving path is defaults-first: experimental tokenization, exact KV routing,
and snapshot routing are all disabled unless explicitly enabled.

## Start with the defaults

If the engine is reachable as `http://ds4-flash:8000`, no `RJ_*` variable is
required. In most deployments, set only the upstream list:

```yaml
environment:
  RJ_UPSTREAM: http://engine-a:8000,http://engine-b:8000
  # RJ_UPSTREAM_TOKEN: ${VLLM_API_KEY} # only for protected engines
```

The proxy listens on `0.0.0.0:8000`; Prometheus metrics listen on
`0.0.0.0:9090`. Invalid settings fail startup instead of silently changing
behavior. Keep secrets in an uncommitted mode-`0600` environment file or a
secret manager.

## The RJ_ prefix, and the retired ones

Settings use `RJ_*`. Two earlier prefixes were retired, `DS4_` and then `MD_`,
and neither is read any more.

A retired variable is not ignored: startup fails and the error names every
stale key it found. That is deliberate. Silently ignoring `MD_ROUTE_ALPHA`
would start a proxy that is healthy, serving, and tuned differently from what
the overlay asked for, which is far harder to notice than a container that
refuses to boot.

Two consequences worth planning for:

- Rename every variable in the same change that ships the new image. A
  half-updated Compose file fails fast, which is the intended behavior, but it
  is still an outage if you discover it during a rolling restart.
- **Rolling back to a pre-rename image needs the old names back.** An image
  built before this cut reads `MD_*` only and will ignore `RJ_*` exactly the
  way this version ignores `MD_*`. Keep the previous Compose revision beside
  the previous image digest in the deployment journal, and roll both together.

Responses carry `X-Ramjet-*` headers, renamed from `X-Mini-Dynamo-*` in the
same change. Anything parsing them — route correlation, the shadow-soak
runner, log pipelines — moves with it. The `ramjet_*` metric names are
deliberately untouched so existing Grafana history and alert rules keep
resolving across the rename.

## Router variables

### Upstreams and routing

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_UPSTREAM` | `http://ds4-flash:8000` | Comma-separated OpenAI-compatible engine URLs. |
| `RJ_UPSTREAM_TOKEN` | unset | Bearer token used for upstream requests and probes. |
| `RJ_AFFINITY` | `prefix` | `prefix` for locality/load scoring; `load` for the load-only baseline. |
| `RJ_ROUTE_ALPHA` | `4` | Non-negative load penalty in the routing score. |
| `RJ_ROUTE_CHUNK_BYTES` | `2048` | Bytes per approximate prefix fingerprint block. |
| `RJ_ROUTE_MAX_PREFIX_BYTES` | `2097152` | Maximum request prefix bytes fingerprinted. |
| `RJ_ROUTE_MAX_OVERLAP_BLOCKS` | `32` | Cap on affinity credit in fingerprint blocks. |
| `RJ_ROUTE_INDEX_CAPACITY` | `100000` | Maximum entries in the approximate locality index. |
| `RJ_ROUTE_LOAD_UNIT_BYTES` | `32768` | Request bytes represented by one reserved load unit. |
| `RJ_ROUTE_MAX_LOAD_UNITS` | `8` | Maximum size-weighted load reservation per request. |
| `RJ_ROUTE_PHASE_AWARE_LOAD` | `false` | Experimental: after the first generated token on a streaming response, reduce the request's size-weighted prefill reservation to its bounded decode reservation. |
| `RJ_ROUTE_DECODE_LOAD_UNIT_TOKENS` | `0` | Requested-output tokens per decode load unit. Zero disables decode weighting and retains one unit. |
| `RJ_ROUTE_DECODE_MAX_LOAD_UNITS` | `min(4, RJ_ROUTE_MAX_LOAD_UNITS)` | Maximum decode reservation; must not exceed the overall route load cap. |
| `RJ_ROUTE_PROJECTED_LOAD` | `false` | Experimental: include each candidate's extra request reservation beyond its decode floor in its approximate route score. |
| `RJ_ROUTE_SPECULATION_MODE` | `off` | `off`, observation-only `shadow`, or `prefer` to use the engine profile as the final score tie-break. |
| `RJ_ROUTE_SPECULATION_PROFILES` | `standard` per upstream | Dense comma-separated `standard` or `mtp` profile for every upstream. Non-off mode requires both profiles. |
| `RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MODE` | `off` | `off`, observation-only `shadow`, or `prefer` to co-locate concurrent cold requests sharing a leading prefix. |
| `RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MIN_BLOCKS` | `8` | Leading approximate fingerprint blocks (16KiB at the default chunk size) that identify one bounded flight. |
| `RJ_ROUTE_PREFIX_SINGLE_FLIGHT_CAPACITY` | `1024` | Maximum concurrently tracked prefix flights; a full table fails open to ordinary routing. |
| `RJ_ROUTE_PREFIX_SINGLE_FLIGHT_MAX_LOAD_DELTA` | `1` | Maximum active load-unit disadvantage allowed when joining the flight's engine. |
| `RJ_ROUTE_JOURNAL` | `false` | Emit privacy-bounded route start/finish records for offline replay. |
| `RJ_MAX_TOKENS_STRIP` | `100000` | Strip client `max_tokens` at or above this compatibility boundary; `0` disables the legacy strip. |
| `RJ_ADVERTISE_CTX_MARGIN` | `16384` | Context tokens withheld when rewriting upstream model metadata. |
| `RUST_LOG` | `info` | Standard tracing filter, for example `ramjet=debug`. |

Decode load uses only the already journaled, low-cardinality effective output
bucket. A bounded bucket reserves its upper edge; `4097+`, unset, and invalid
limits reserve the configured maximum rather than trusting an unbounded or
malformed request. The reservation is a floor on each candidate's prefill
estimate, not an additive token count, and exact-prefix recomputation cannot
drop below it. On a streaming response the phase-aware boundary releases the
prefill portion at the first generated token while retaining the decode floor.
Non-streaming responses retain their dispatch reservation until completion
because the proxy has no protocol-visible first-token boundary. The zero token
quantum is an instant behavior rollback.

Profile-aware speculation routing uses only the effective output-limit bucket:
requests through 256 tokens prefer `mtp`, larger, missing, or malformed limits
prefer `standard`, and non-generation endpoints stay neutral. `shadow` records
the bounded counterfactual without changing placement. `prefer` is deliberately
a final tie-break after serving health, weighted locality/load score, and raw
prefix overlap; it cannot trade a warmer prefix or a less-loaded replica for an
engine profile. `RJ_ROUTE_SPECULATION_MODE=off` is the instant rollback.

`GET /health` returns opaque replica ordinals, serving health, DSpark
reliability state, inflight work, load units, and index size. It returns `200 ok` when every replica is healthy,
`200 degraded` when at least one can serve, and `503 unhealthy` when none can
serve. Successful proxied responses include `X-Ramjet-Upstream` with an
opaque replica ordinal.

Projected-load scoring keeps all-cold and other equal-cost choices unchanged.
When approximate overlap makes a request cheaper on one replica, its score uses
`affinity - alpha * (active_load + request_load - 1)`. This can preserve a
large warm prefix that the bounded affinity credit alone would spill cold, but
it also strengthens stale approximate locality. Keep it off until a guarded
cache/load conflict test proves service-time improvement rather than only more
prefix hits.

Prefix single-flight closes the interval before a successful response has
published approximate cache residency: the first cold request owns a bounded
leading-prefix flight, and concurrent followers may join its engine. Flights
exist only while their requests are alive, carry no prompt bytes, are capped by
entry count, retarget after dispatch failover, and never override exact routing
or a prefix already known warm. The load-delta gate prevents coalescing from
turning one engine into an unbounded queue. Start with `shadow`; `prefer` is the
only mode that changes placement.

Serving admission fails open. A readiness probe competes with request traffic
for the same engine capacity, so a saturated fleet stops answering probes
before it stops serving, and all replicas saturate together. Two rules keep
that from turning a busy stack into a total outage:

- A probe timeout or connection failure does not fence a replica that
  completed a real request in the last 30 seconds; the probe failure is still
  counted in `ramjet_upstream_probe_failures_total`, and the suppression in
  `ramjet_upstream_probe_suppressed_total`. A probe the engine *answers* with
  an error, and any failed request, still fence it immediately.
- When no replica is healthy, requests are dispatched anyway rather than shed,
  which is visible in `ramjet_route_fail_open` and
  `ramjet_route_fail_open_dispatches_total`. `/health` and
  `ramjet_upstream_up` keep reporting the true admission state throughout.
  Deliberate fences are never bypassed: a DSpark quarantine, an unmet
  compatibility admission, and an idle-drain park still refuse traffic, and a
  fleet in those states still returns `503`.

### DSpark reliability guard (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_DSPARK_GUARD_MODE` | `off` | `off`, telemetry-only `observe`, or enforcing `quarantine`. |
| `RJ_DSPARK_GUARD_INTERVAL_MS` | `5000` | Native engine `/metrics` polling interval, from 1–60 seconds. Missed ticks delay instead of bursting. Each request has a separate two-second timeout and 4MiB body cap. |
| `RJ_DSPARK_GUARD_CONSECUTIVE_WINDOWS` | `3` | Consecutive qualifying zero-acceptance windows required, from 2 through 12. |
| `RJ_DSPARK_GUARD_MIN_PROPOSED_TOKENS` | `256` | Minimum proposed draft tokens in each qualifying window. |
| `RJ_DSPARK_GUARD_EXPECTED_POSITIONS` | `5` | Exact speculative positions required in every sample; use `5` for fixed K5. |
| `RJ_DSPARK_GUARD_STATE_PATH` | unset | Required only for `quarantine`: normalized absolute path to a pre-created mode-0600 durable state file in a protected mode-0700 directory. |
| `RJ_DSPARK_GUARD_STATE_OWNER_UID` | `0` | Required owner UID for the durable state file and directory. |
| `RJ_DSPARK_GUARD_STATE_GROUP_GID` | `0` | Required group GID for the durable state file and directory. |

The guard detects the production-shaped DSpark failure where an active engine
continues proposing draft work but accepts exactly zero tokens at every K5
position across multiple windows. Until a live image proves another shape, the
parser fails closed unless each counter family and position has exactly one
coherent label domain; this prevents one shard reset from being hidden by
another shard's increment. Missing, partial, duplicate, multi-series,
mismatched-label, non-finite, reset, idle, oversized, or internally
inconsistent counters break the streak and never trigger quarantine. `observe`
records the same decision without changing routing.

`quarantine` atomically removes the affected replica from new serving attempts;
the healthy peer remains available, and two quarantined replicas produce a 503
without dialing either engine. Quarantine is sticky across LB crashes and
container recreates: the file is fsynced before the serving fence is published,
and a persistence failure itself fails closed. Re-admission first durably
removes the record and then requires a different canonical SHA-256 commitment
of the compatibility-attested EngineCore incarnation set. A frontend-only
identity change cannot rearm it. Enforcing mode therefore requires
`RJ_UPSTREAM_ADMISSION_MODE=compatibility` and the protected state path. Raw
incarnations and upstream URLs are never stored. The bounded schema-v1 file
also precommits a runtime-dirty marker. After an unclean LB exit or a failed
store mutation, every replica without an existing record starts fenced and its
currently attested EngineCore is durably quarantined before it can serve; an
ordinary clean LB shutdown clears the marker. The file
contains only opaque replica ordinals and SHA-256 commitments. Start with
`observe` and qualify both false-positive behavior and counter shape before
enabling enforcement.

Inspect each replica's fixed `reliability_state` and `quarantined` fields in
`/health`, plus `ramjet_dspark_guard_state`,
`ramjet_dspark_guard_windows_total`, and
`ramjet_dspark_guard_quarantines_total`. Durable publication failures use the
fixed `persistence_failure` state and
`ramjet_dspark_guard_persistence_failures_total`. Valid windows also export strict
acceptance, effective tokens per draft step, and per-position acceptance ratios
with a separate measurement-available gauge. Replica labels are opaque ordinals;
no process identity, metric payload, prompt, or completion content is exposed.

### Opaque session affinity (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_SESSION_AFFINITY_MODE` | `off` | `off` or observation-only `shadow`; shadow never changes the served replica. |
| `RJ_SESSION_AFFINITY_KEY` | unset | Independent 32–256-byte HMAC key required in shadow mode. |
| `RJ_SESSION_AFFINITY_BONUS_BLOCKS` | `4` | Counterfactual cache-equivalent bonus, at most `RJ_ROUTE_MAX_OVERLAP_BLOCKS`. |
| `RJ_SESSION_AFFINITY_MAX_LOAD_DELTA` | `0` | Maximum load above the least-loaded healthy replica admitted for a counterfactual affinity target. |

For one bounded `X-Session-ID`, shadow mode uses keyed rendezvous hashing to
derive a stable primary and secondary from the configured upstream ordinals.
It then records whether the bounded bonus would retain that pair without
crossing the load guardrail. Missing, duplicate, empty, or larger-than-256-byte
session headers fail closed. The header is stripped before upstream dispatch;
the raw ID, HMAC scores, and key never enter logs, metrics, or the route
journal. Journal v5 stores only a policy version, bounded policy parameters,
outcomes, and opaque ordinals. `bench/route_replay.py` independently reproduces
the decision, reports record mismatches, filters with `--session-affinity`, and
sweeps `--session-bonus-blocks` / `--session-max-load-delta` without live
traffic.

This first slice is stateless prospective assignment, not learned
"previous-replica" state, and it has no placement mode. Upstream list order and
cardinality are part of assignment identity; reordering the list or rotating
the key deliberately remaps sessions. Keep the affinity key separate from the
exact-canary and snapshot secrets.

### Idle drain and engine parking (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_IDLE_DRAIN_MODE` | `off` | `off`, `observe` (evaluate and publish, fence nothing), or `drain` (fence a parked replica from routing). `drain` needs at least two upstreams. |
| `RJ_IDLE_DRAIN_RELEASE` | `fleet-idle` | `fleet-idle` releases only when nothing is running anywhere; `utilization` releases an individually quiet replica while its peers serve. |
| `RJ_IDLE_DRAIN_UPSTREAM_IDLE_AFTER_SECONDS` | `300` | Zero-inflight time before one replica is releasable under `utilization`. At least 60. |
| `RJ_IDLE_DRAIN_RESUME_LOAD_PER_REPLICA` | `4` | Mean in-flight per serving replica that resumes every parked replica, and the anti-flap bound. |
| `RJ_IDLE_DRAIN_IDLE_AFTER_SECONDS` | `900` | Quiet period before the fleet counts as idle. At least 60. |
| `RJ_IDLE_DRAIN_MIN_WARM` | `1` | Replicas that must stay warm. Clamped to at least one; an unhealthy replica never counts toward it. |
| `RJ_IDLE_DRAIN_COOLDOWN_SECONDS` | `300` | Minimum spacing between drain transitions. Never applies to resuming. |
| `RJ_IDLE_DRAIN_GRACE_SECONDS` | `30` | Quiet time a fenced replica must accumulate before it is safe to park. |
| `RJ_IDLE_DRAIN_INTERVAL_SECONDS` | `15` | Policy evaluation period. |
| `RJ_IDLE_DRAIN_ACTUATOR` | `sleep` | `off` publishes intent only; `sleep` parks the engine with vLLM sleep mode. Can only act in `drain` mode. |
| `RJ_IDLE_DRAIN_SLEEP_LEVEL` | `1` | `1` offloads weights to host RAM; `2` discards them and re-reads the model on wake. |
| `RJ_IDLE_DRAIN_MAX_PARKED` | `1` | Replicas that may hold offloaded weights at once. Must be fewer than the upstream count in `drain` mode. |

The knobs are expressed in seconds. There is deliberately no `_MS` alias, so a
stale millisecond-spelled variable is inert rather than a thousand-fold
misconfiguration.

The policy is asymmetric on purpose. Draining is gated by the cooldown and the
warm floor; resuming is immediate and bypasses every rate limit, because being
short of capacity costs a cold start while parking late costs watt-minutes. The
warm floor is restored on every tick rather than only on traffic: a park that
was safe when it was made becomes unsafe if the replica that stayed warm later
fails, and during an idle window no request would arrive to notice.

`RJ_IDLE_DRAIN_ACTUATOR=sleep` is the first configuration in which the balancer
carries out its own decision. That is allowed because vLLM sleep mode is not a
Docker socket: `POST /sleep` and `POST /wake_up` authenticate with the same
`RJ_UPSTREAM_TOKEN` the readiness probe already uses, so the authority is
reversible and engine-scoped. The rule against giving the balancer a Docker
socket still stands for container stop/start, which this path does not need.

The engine must be started with `--enable-sleep-mode` and `VLLM_SERVER_DEV_MODE=1`;
the second is what registers `/sleep`, `/wake_up`, and `/is_sleeping`. Both are
startup-time, so enabling parking costs an engine restart, and dev mode
registers vLLM's whole dev route group — keep those ports on the Compose
network and never expose one through a public listener.

A parked or waking replica stays fenced from routing regardless of what the
policy intends, because the policy unfences the instant it wants a replica
running and the weights may still be in host memory. A sleep or wake that
fails leaves the replica in `unknown`, which fences it and schedules a
`/is_sleeping` reconciliation rather than guessing; failures never reach
`/health` or upstream health, because the engine is still serving and a
balancer that cannot park an idle replica has lost an optimisation, not a
capability.

`RJ_IDLE_DRAIN_MAX_PARKED` bounds **host** memory, not GPU memory, and the
warm floor cannot express that because it counts replicas rather than bytes.

A direct measurement on node06 (2026-08-20) is worth knowing before tuning it.
Level-1 sleep on one Qwen3.8-27B TP2 engine freed 87,890MiB of VRAM per GPU in
23.2s and woke in 894ms with inference working 132ms later — but it took about
38GiB of host memory and did not release it on wake. Available memory stayed at
4.5GiB while the engine was awake and serving; only stopping the container
recovered it.

Read the cap as "how many replicas may ever park during a container's
lifetime", not "how many may be parked at once". Raising it commits another
~38GiB permanently, not transiently.

The same run found that a sleeping engine **hangs** rather than refusing: a
request issued to it produced no response within ten seconds. Routing into a
sleeping replica stalls requests until their own timeouts instead of failing
fast, which is why the park fence is an invariant rather than a nicety.

Observe mode never actuates even with the default actuator, so it remains the
consequence-free way to qualify the policy against real traffic.

#### Choosing a release trigger

These are different products, not two tunings of one. **Fleet idleness** is a
statement about the whole deployment and is safe by construction: nothing is
running anywhere, so parking costs nothing. It is the default for that reason.

It is also, on a deployment whose traffic never stops, a policy that never
fires. node06 was measured with a longest quiet gap of 20 seconds against a
900-second window while six of eight GPUs sat at 0% utilization drawing about
620W — all traffic co-located on one replica by the prefix router, which is the
router working correctly. No setting of `RJ_IDLE_DRAIN_IDLE_AFTER_SECONDS`
recovers that power, because the fleet is genuinely busy.

**Utilization** releases a replica that has been individually quiet. Under it,
resume stops keying on request arrival — meaningless when requests never stop —
and keys on load pressure instead: when mean in-flight per serving replica
reaches `RJ_IDLE_DRAIN_RESUME_LOAD_PER_REPLICA`, every parked replica comes
back, ignoring the cooldown as every move toward capacity does.

That same threshold is the anti-flap bound. A release is refused when it would
push the remaining replicas to or past it, so the policy can never park a
replica the next tick would have to wake — each such cycle costs a full weight
transfer in both directions and is strictly worse than never parking.

Two costs are real here and are not present under fleet idleness. A burst
arrives to fewer engines and waits on a wake. And sleeping discards the
replica's KV cache, so a session returning to a parked replica re-prefills;
that is why the per-replica window is floored at 60s and defaults to 300s, since
a replica between two turns of one conversation is waiting rather than idle.

Target selection differs between the triggers, and the difference matters.
Fleet idleness picks the highest-indexed healthy replica, which is sound
because an idle fleet's replicas are interchangeable. Utilization picks the
replica that has been quiet *longest* and requires it to be at zero inflight:
on node06 the highest-indexed replica is the one holding the entire workload,
so selecting by index would park production and leave three idle engines warm.

Tune from `ramjet_idle_drain_load_per_replica`, which exports the decision
input directly.

### Tokenization (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_TOKENIZER_MODE` | `off` | `off`, `remote-shadow`, or `local-shadow`; shadow modes never change the approximate decision alone. |
| `RJ_TOKENIZER_PATH` | unset | `tokenizer.json` path; required by `local-shadow`. |
| `RJ_TOKENIZER_SHA256` | unset | Expected 64-character artifact SHA-256; required by `local-shadow`. |
| `RJ_TOKENIZER_PROFILE` | `deepseek-v4-r34` | Prompt-renderer profile; one of the labels registered in `src/model/`. Unknown values are rejected at startup. See [models.md](models.md). |
| `RJ_CHAT_TEMPLATE_PATH` | unset | `tokenizer_config.json` supplying the Jinja chat template; required by template-driven profiles. |
| `RJ_CHAT_TEMPLATE_SHA256` | unset | Expected chat-template SHA-256. Required with, and only with, `RJ_CHAT_TEMPLATE_PATH`. |
| `RJ_TOKENIZER_MIN_BYTES` | `32768` | Minimum request bytes admitted to shadow tokenization. |
| `RJ_TOKENIZER_MAX_BYTES` | `2097152` | Maximum request bytes admitted to shadow tokenization. |
| `RJ_TOKENIZER_WORKERS` | `1` | Bounded blocking workers for local tokenization. |
| `RJ_TOKENIZER_QUEUE_CAPACITY` | `8` | Non-blocking remote-shadow queue capacity. |
| `RJ_TOKENIZER_TIMEOUT_MS` | `2000` | Per-tokenization timeout. |

`remote-shadow` calls the selected engine's authenticated `/tokenize` endpoint
after request completion. `local-shadow` compares bounded local token IDs with
that remote authority in memory. Prompt text and token IDs are not retained in
logs, metrics, or journals.

### Exact route evaluation (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_EXACT_ROUTE_MODE` | `off` | `off`, observation-only `shadow`, or canary `placement`. |
| `RJ_EXACT_ROUTE_MANIFEST_PATH` | unset | Compatibility manifest; required when exact routing is enabled. |
| `RJ_EXACT_ROUTE_MANIFEST_SHA256` | unset | Expected manifest SHA-256; required when exact routing is enabled. |
| `RJ_SERVING_RUNTIME_MANIFEST_PATH` | unset | Separate serving-runtime manifest linked to the compatibility-manifest digest; required by `compatibility` admission and safe to stage while admission remains `http`. |
| `RJ_SERVING_RUNTIME_MANIFEST_SHA256` | unset | Expected serving-runtime manifest SHA-256; must be configured together with its path. |
| `RJ_EXACT_ROUTE_WORKERS` | `4` | Bounded exact-index lookup workers. |
| `RJ_EXACT_ROUTE_TIMEOUT_MS` | `250` | Exact pre-route evaluation timeout. |
| `RJ_EXACT_ROUTE_MIN_GAIN_TOKENS` | `8192` | Minimum exact cached-token gain required to move a canary request. |
| `RJ_EXACT_ROUTE_MAX_LOAD_DELTA` | `0` | Maximum additional load allowed on an exact winner. |
| `RJ_EXACT_ROUTE_CANARY_BPS` | `0` | Stable placement cohort size in basis points, from `0` to `10000`. Zero is instant rollback. |
| `RJ_EXACT_ROUTE_CANARY_KEY` | unset | 32–256-byte HMAC key required when the placement cohort is nonzero. |
| `RJ_UPSTREAM_ADMISSION_MODE` | `http` | `http` admits a replica after `/v1/models`. `compatibility` additionally requires one atomic `/v1/mini-dynamo/identity` response to match the pinned manifest. |
| `RJ_UPSTREAM_ADMISSION_TIMEOUT_MS` | `5000` | Absolute timeout, at most 30 seconds, for the atomic serving-identity request. Independent of tokenization timeouts. |

Exact routing requires `RJ_TOKENIZER_MODE=local-shadow`, a pinned manifest,
and exactly one inventory source: direct KV events or snapshot companions.
Placement additionally requires `RJ_AFFINITY=prefix`; snapshot inventories are
shadow-only. Any timeout, attestation failure, event gap, revision change, or
missing `X-Session-ID` preserves the approximate route.

For vLLM hybrid caches, Ramjet learns the bounded cache-group index and
`kv_cache_spec_kind` carried by KV events. Reusable full/MLA groups feed the
exact prefix index; Mamba, sliding-window, local, encoder, and cross-attention
groups are observed but excluded. An unknown kind or a group seen before its
kind is learned keeps exact comparison available in `shadow` while dynamically
fencing `placement`. Untagged legacy event streams retain their existing
placement behavior. `/health` reports only content-free group counts plus
`placement_ready` under each replica's `exact_inventory`; no group state is
folded into ordinary upstream health.

When exact placement applies, admission reservations are recomputed from the
exact warm-prefix overlap instead of the approximate block estimate that was
derived before the inventory was consulted. A request whose prefix is already
resident therefore reserves proportionally less capacity, bounded by the same
`RJ_ROUTE_LOAD_UNIT_BYTES` quantum and `RJ_ROUTE_MAX_LOAD_UNITS` cap. The
recompute is atomic across healthy candidates and fails closed: if any healthy
candidate lacks a trusted overlap, every original reservation is preserved. It
never changes the selected replica for the request being recomputed — placement
is decided first, and the gain/load gates still run against the pre-route
estimate. It does change the load accounting that *later* decisions read: the
reservation is what `acquire_if_healthy` adds to the upstream's load, which
becomes the next request's alpha-weighted load term. Steering warm work to an
engine that now reports lower load is the intended effect, but it is a feedback
loop, not a no-op.

This applies only to `RJ_EXACT_ROUTE_MODE=placement`, and there whether or not
the exact winner actually moves the request. `shadow` stays strictly
observation-only: it never alters a reservation.

The recompute can also *raise* a reservation, up to `RJ_ROUTE_MAX_LOAD_UNITS`.
If the approximate prefix index is stale and the engine has actually evicted
the prefix, exact overlap is zero and the request correctly reserves the cold
cost the approximate estimate understated. Expect `ramjet_upstream_load_units`
to step up on the first placement rollout; watch the upstream-split panel and
compare against the journal rather than assuming a regression.

Compatibility admission is an independent serving gate. It requires
`RJ_TOKENIZER_MODE=local-shadow` plus the SHA-pinned manifest so local golden
validation exists, the separately SHA-pinned serving-runtime manifest, and at
least two upstreams. It does not enable exact routing. The schema-v2 runtime
manifest binds the expected EngineCore cardinality, KV-event publisher
configuration, complete normalized serving argv, selected non-secret
environment, runtime package versions, and exact launcher/NCCL artifact hashes;
its `compatibility_manifest_sha256` must equal the renderer/tokenizer manifest
pin exactly.
`bench/serving_runtime_image_probe.py --output PATH` derives canonical process
and KV-event evidence from the real immutable launcher without a GPU or
network. It preserves the reviewed template key/path shape, rejects secret-like
or malformed capture, and writes atomically; the generated diff, semantic
Compose validation, both service shapes, and every new SHA pin still require
review before rollout. The external engine build does not yet emit or sign
this manifest itself.
A mismatching replica is removed from ordinary serving until a later probe
passes; the other healthy replica remains available. Keep the default `http`
mode unless every upstream implements the identity contract below. Inspect
`compatibility_attested` in `/health`,
`ramjet_upstream_compatibility_admitted`, and
`ramjet_upstream_admission_checks_total` before opting in.

The identity endpoint must capture the frontend and every expected EngineCore
atomically and return a bounded schema-v3 document. Each incarnation is an
opaque value of 1–256 ASCII alphanumeric/`.`/`_`/`:`/`-` bytes. ramjet
validates these values but never logs, labels, journals, or retains the raw
values. Enforcing DSpark reliability mode retains only a canonical SHA-256
commitment of the sorted EngineCore incarnation set. It persists that opaque
commitment before fencing traffic, so neither a frontend restart nor an LB
restart lets the same EngineCore clear its own quarantine.

```json
{
  "schema_version": 3,
  "incarnation": {
    "frontend": "boot-id:frontend-pid:process-start",
    "engine_core": ["boot-id:core-pid:process-start"]
  },
  "model": {"id": "...", "root": "...", "max_model_len": 393216},
  "engine": {
    "version": "...",
    "image_digest": "sha256:...",
    "core_process_count": 1,
    "kv_events": {
      "enable_kv_cache_events": true,
      "publisher": "zmq",
      "endpoint": "tcp://*:5557",
      "replay_endpoint": "tcp://*:5558",
      "buffer_steps": 10000,
      "hwm": 100000,
      "max_queue_size": 100000,
      "topic": ""
    }
  },
  "tokenizer": {"sha256": "..."},
  "renderer": {"profile": "..."},
  "runtime": {
    "argv_sha256": "...",
    "environment_sha256": "...",
    "packages_sha256": "...",
    "artifacts_sha256": "..."
  }
}
```

Compatibility mode fences all replicas at LB startup and probes those initially
fenced replicas with at most eight probes in flight. It therefore returns 503
until at least one atomic identity succeeds. Later rounds recheck unhealthy
replicas first and fence each healthy replica for its own bounded check. If only
one admitted peer remains, that peer keeps serving during its bounded atomic
recheck; a match refreshes admission, while mismatch or unavailability fences it
immediately after the check. This avoids creating an outage merely to begin a
probe without allowing stale identity to remain admitted indefinitely.

For the node06 vLLM stack,
`deploy/dspark_0731/docker-compose.compatibility-identity.yaml` provides the
candidate engine endpoint in-process through vLLM's `--middleware` extension.
It authenticates independently with the engine bearer so middleware ordering
cannot expose the document, and startup verifies the live vLLM package, served
model, context length, and tokenizer digest. The overlay is deliberately
separate from the base Compose, pins the immutable r34 image, and does not
enable LB admission. The first authenticated identity call exercises the real
initialized `/v1/models` and all committed `/tokenize` goldens through the
inner ASGI app, brackets them with vLLM health, and caches a complete match for
that frontend process. The same proof requires runtime-manifest schema v2's
complete normalized argv, allow-listed non-secret environment, runtime package
versions, and exact launcher/NCCL artifact hashes, plus the exact pinned vLLM
client and process-manager types, a stable direct-child EngineCore incarnation,
the live typed KV-event configuration, and exact child-owned wildcard listeners
for the event and replay ports. The endpoint returns only the four runtime
evidence digests. Later calls still check health, process identity,
configuration, and socket ownership. The 4s inner deadline is
below the LB's 5s control deadline; cancellation sends an internal disconnect
and waits for vLLM's child tasks so a timeout cannot orphan tokenization work.
The one-time proof intentionally appears in vLLM's HTTP metrics as one models
request and ten tokenize requests; it never reaches inference scheduling.
This verifies the live renderer/model-root claims and binds the serving launch,
packages/artifacts, and EngineCore process/configuration/listening sockets. It
does not prove publisher-thread
liveness, event advancement, replay completeness, or the complete runtime
bundle. Validate it with `validate-serving-identity-compose.py` and roll one
engine at a time. The candidate remains diagnostic: keep the LB in `http` mode
until a live event/replay qualification and the remaining runtime-bundle work
are complete.

Cold exact misses also emit a strictly observation-only projected-balance
counterfactual. `ramjet_exact_route_projected_balance_total` adds each
replica's exact resident tokens to a conservative, current-request-equivalent
translation of its bounded active load. This makes an in-flight cold prefill
visible before its KV events arrive. The estimate is token pressure, not a
claim that decode work will become resident KV, and it never changes placement.
`bench/cachebench.py` captures its fixed `kept_selected`, `would_balance`,
`kept_delta_gate`, `kept_load_gate`, and `fallback` outcomes alongside the
existing raw-residency shadow.

### Direct KV-event inventory (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_KV_EVENT_MODE` | `off` | `off` or observation-only `shadow`. |
| `RJ_KV_EVENT_LIVE_ENDPOINTS` | unset | Comma-separated internal `tcp://host:port` endpoints, one per upstream. |
| `RJ_KV_EVENT_REPLAY_ENDPOINTS` | unset | Matching replay endpoints, one per upstream. |
| `RJ_KV_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `RJ_KV_EVENT_REPLAY_LIMIT` | `1024` | Maximum replay batches accepted during recovery. |
| `RJ_KV_EVENT_REPLAY_TAIL_LIMIT` | `64` | Maximum bounded replay tail. |
| `RJ_KV_EVENT_TIMEOUT_MS` | `5000` | Connect/replay operation deadline. |
| `RJ_KV_EVENT_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `RJ_KV_EVENT_RECONNECT_MAX_MS` | `10000` | Maximum reconnect backoff. |

Inventories start untrusted and fence on disconnect or invalid replay. Sparse
vLLM scheduler sequence numbers are valid; duplicate, decreasing,
out-of-range, or incomplete replay remains invalid. Raw hashes and token IDs
never enter logs, metrics, or journals.

### Snapshot inventory client (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_SNAPSHOT_ROUTE_MODE` | `off` | `off` or observation-only `shadow`. |
| `RJ_SNAPSHOT_ROUTE_SOCKET_PATHS` | unset | Unix socket path per upstream. |
| `RJ_SNAPSHOT_ROUTE_COMPANION_UIDS` | unset | Non-root companion UID per upstream. |
| `RJ_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS` | unset | Session secret path per upstream. |
| `RJ_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS` | unset | Digest secret path per upstream. |
| `RJ_SNAPSHOT_ROUTE_ATTESTATION_PATHS` | unset | Engine attestation path per upstream. |
| `RJ_SNAPSHOT_ROUTE_GROUPS` | unset | `data_parallel_rank:group_index` per upstream. |
| `RJ_SNAPSHOT_ROUTE_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `RJ_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS` | `1000` | Attestation refresh interval. |
| `RJ_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS` | `30000` | Absolute connection/snapshot attempt deadline. |
| `RJ_SNAPSHOT_ROUTE_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `RJ_SNAPSHOT_ROUTE_RECONNECT_MAX_MS` | `5000` | Maximum reconnect backoff. |

All per-upstream lists must match `RJ_UPSTREAM` in length. Socket, session
secret, digest secret, and attestation paths must be normalized, absolute, and
distinct across every authority domain. Use the validated production overlay
in [`deploy/dspark_0731`](../deploy/dspark_0731/README.md); do not improvise
snapshot permissions or expose its sockets over TCP.

## Snapshot companion variables

The separate `ramjet-snapshot-companion` binary is experimental and
defaults to `off`, where it needs no files, engine, or listener. Serve mode is
one companion per engine and should be configured through the repository's
validated Compose overlay.

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_SNAPSHOT_COMPANION_MODE` | `off` | `off` or `serve`. |
| `RJ_SNAPSHOT_SOCKET_PATH` | unset | Required serve-mode Unix socket path. |
| `RJ_SNAPSHOT_COMPANION_UID` | unset | Required non-root process UID. |
| `RJ_SNAPSHOT_CLIENT_UID` | unset | Required, distinct non-root LB UID. |
| `RJ_SNAPSHOT_SECRET_PATH` | unset | Required 32-byte session secret path. |
| `RJ_SNAPSHOT_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `RJ_SNAPSHOT_LIVE_ENDPOINTS` | unset | Required live KV endpoint; standalone mode accepts exactly one. |
| `RJ_SNAPSHOT_REPLAY_ENDPOINTS` | unset | Matching replay endpoint. |
| `RJ_SNAPSHOT_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `RJ_SNAPSHOT_MAX_CLIENTS` | `2` | Active authenticated clients; standalone serve mode requires `2`. |
| `RJ_SNAPSHOT_TAIL_QUEUE_CAPACITY` | `1024` | Maximum queued tail entries. |
| `RJ_SNAPSHOT_TAIL_QUEUE_MAX_BYTES` | `16777216` | Maximum queued tail payload bytes. |
| `RJ_SNAPSHOT_DEADLINE_MS` | `3000` | Snapshot-phase deadline. |
| `RJ_SNAPSHOT_TAIL_IDLE_DEADLINE_MS` | `30000` | Tail idle/write budget. |
| `RJ_SNAPSHOT_SHUTDOWN_DEADLINE_MS` | `5000` | Supervisor drain deadline. |
| `RJ_SNAPSHOT_MAX_FRAME_BYTES` | `33554432` | Maximum snapshot frame bytes. |
| `RJ_SNAPSHOT_MAX_TAIL_FRAME_BYTES` | `8392704` | Maximum tail frame bytes. |
| `RJ_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES` | `8388608` | Maximum decoded event-batch payload; must be smaller than a tail frame. |
| `RJ_SNAPSHOT_MAX_BATCH_EVENTS` | `4096` | Maximum events per decoded batch. |
| `RJ_SNAPSHOT_DIGEST_SECRET_PATH` | unset | Required, distinct digest secret path. |
| `RJ_SNAPSHOT_ATTESTATION_PATH` | unset | Required, distinct authenticated engine identity path. |
| `RJ_SNAPSHOT_ATTESTATION_REFRESH_MS` | `1000` | Engine identity refresh interval. |
| `RJ_SNAPSHOT_BLOCK_SIZE` | unset | Required engine KV block size. |
| `RJ_SNAPSHOT_ATTENTION_KIND` | `mla` | `full`, `mla`, or `sink_full`. |
| `RJ_SNAPSHOT_DATA_PARALLEL_RANK` | `0` | Group data-parallel rank. |
| `RJ_SNAPSHOT_GROUP_INDEX` | `0` | Group index. |
| `RJ_SNAPSHOT_CONNECT_TIMEOUT_MS` | `2000` | Engine KV-event connect timeout. |
| `RJ_SNAPSHOT_REPLAY_TIMEOUT_MS` | `30000` | Engine replay timeout. |
| `RJ_SNAPSHOT_REPLAY_LIMIT` | `10000` | Maximum replay batches. |
| `RJ_SNAPSHOT_REPLAY_TAIL_LIMIT` | `1024` | Maximum replay tail batches. |
| `RJ_SNAPSHOT_RECONNECT_MIN_MS` | `250` | Initial source reconnect backoff. |
| `RJ_SNAPSHOT_RECONNECT_MAX_MS` | `5000` | Maximum source reconnect backoff. |
| `RJ_SNAPSHOT_METRICS_BIND` | `127.0.0.1:9091` | Loopback-only TCP metrics address. |
| `RJ_SNAPSHOT_METRICS_SOCKET_PATH` | unset | Metrics-only Unix socket; mutually exclusive with `RJ_SNAPSHOT_METRICS_BIND`. |
| `RJ_SNAPSHOT_METRICS_GROUP_GID` | unset | Required dedicated non-root group when using a metrics Unix socket. |

The metrics Unix socket must use a separate, setgid authority directory and a
group that is not the snapshot/session group. Scrapers may join only the
metrics group.

## Attestation provisioner variables

`ramjet-attestation-provisioner` accepts no arguments and is silent on
success. It requires all settings below except the age override.

| Variable | Default | Description |
| --- | --- | --- |
| `RJ_SNAPSHOT_ENGINE_METADATA_PATH` | required | Fresh, protected schema-v1 engine metadata input. |
| `RJ_SNAPSHOT_DIGEST_SECRET_PATH` | required | Digest secret used to authenticate the output. |
| `RJ_SNAPSHOT_ATTESTATION_PATH` | required | Atomically published attestation output. |
| `RJ_SNAPSHOT_SECRET_OWNER_UID` | required | Exact output/input owner UID. |
| `RJ_SNAPSHOT_SECRET_GROUP_GID` | required | Exact output group GID. |
| `RJ_SNAPSHOT_ATTESTATION_MAX_AGE_MS` | `30000` | Maximum metadata age; bounded to five minutes. |

## Metrics and route journals

Prometheus is exposed at `/metrics`; `/metrics/upstream/{ordinal}` proxies a
single engine's metrics without exposing its address. The most useful router
families are:

- `ramjet_upstream_up`, inflight, and load gauges for availability.
- `ramjet_route_fail_open` and `ramjet_route_fail_open_dispatches_total`
  for intervals served while no replica passed its admission probe, and
  `ramjet_upstream_probe_suppressed_total` for probe failures outvoted by
  recent serving traffic.
- `ramjet_route_decisions_total` for route distribution.
- `ramjet_cache_requests_total` and prompt/cached token counters for observed
  cache outcomes.
- `ramjet_cache_ttft_seconds` for streaming time to first generated content.
- `ramjet_session_affinity_total` for bounded prospective pair, health, load,
  and score outcomes when session shadow mode is enabled.
- `ramjet_dspark_guard_state` and `ramjet_dspark_guard_windows_total` for
  DSpark degeneration observation; strict/per-position acceptance and
  effective-tokens-per-step gauges describe each valid window, while
  quarantine transitions use a separate fixed-reason counter.

With `RJ_ROUTE_JOURNAL=true`, replay bounded decision snapshots without
changing live traffic:

```bash
docker logs ds4-loadbalancer 2>&1 | \
  python3 bench/route_replay.py - --alphas 1,2,4,8 --caps 8,16,32,64
```

Audit the cost actually delivered by the observed routes, optionally against
separate TTFT and TPOT SLOs:

```bash
docker logs ds4-loadbalancer 2>&1 | \
  python3 bench/serving_cost_audit.py - \
    --ttft-slo-ms 2000 --tpot-slo-ms 50 --gpu-count 8
```

The audit follows the measured-delivery principle in
[MOSAIC](https://arxiv.org/abs/2608.10605) and the separate TTFT/TPOT goodput
constraints in [DistServe](https://www.usenix.org/conference/osdi24/presentation/zhong-yinmin).
Its TTFT-per-uncached-token statistic includes queueing and transport, so it is
a serving-cost signal rather than isolated engine throughput.

Route-journal v7 adds a fixed-cardinality `output_limit` observation without
changing request admission or routing. Policy version 1 uses these endpoint
field precedences:

- OpenAI chat: `max_completion_tokens`, then legacy `max_tokens`.
- OpenAI completions and Anthropic messages: `max_tokens`.
- OpenAI Responses: `max_output_tokens`.

Optional OpenAI cap fields set to JSON `null` are treated as absent caller
policy; a non-null malformed preferred field remains invalid instead of
silently selecting a fallback. Requested/effective here mean the request field
before/after ramjet's compatibility shim, not an engine-resolved default.
For example, current vLLM Completions normalizes an absent or null `max_tokens`
to its default of 16, but this journal intentionally records `unset` rather
than misrepresenting that server default as a caller-selected budget. Chat
still falls back from null `max_completion_tokens` to a non-null legacy
`max_tokens`.
Explicit `stream: null` is non-streaming for the four supported APIs, whose
current vLLM schemas model it as optional false. See the upstream
[chat protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/chat_completion/protocol.py),
[Completions protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/completion/protocol.py),
[Responses protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/openai/responses/protocol.py),
and [Anthropic protocol](https://github.com/vllm-project/vllm/blob/main/vllm/entrypoints/anthropic/protocol.py).

Only `unset`, `invalid`, `1_64`, `65_256`, `257_1024`, `1025_4096`, and
`4097_plus` are retained. The record contains requested and post-shim effective
buckets/sources, one of four fixed strip actions, and `stream` classified as
unset, non-streaming, streaming, or invalid. It never contains the requested
number. A stripped preferred chat field can therefore be distinguished from
the bounded fallback actually forwarded upstream.

`serving_cost_audit.py` schema v2 joins those observations to bounded endpoint,
stream, initial-load bucket, completion-token, total/decode duration, TTFT,
TPOT, failure, and client-disconnect summaries. Headline latency/token
distributions include successful completions only; each outcome has a separate
distribution so early disconnects cannot look like cheaper successful work.
Journal v8 adds `request_load_units` to the finish record: the reservation
actually acquired for the served upstream. The start record's candidate
estimates are written pre-exact, so under placement mode the finish value
differs from them on every recomputed request, not only on failover; failover
additionally makes the reserving candidate differ from the selected one. The
audit therefore prefers the finish value and falls back to the pre-route
candidate estimate only for v1-v7 traces, where that fallback systematically
over-reports warm requests under placement. It is a bounded integer and carries
no prefix identity.
Journal v9 adds the bounded `projected_load` policy bit to the start record so
offline replay can reproduce whether candidate-specific request cost affected
the approximate score.
Journal v1-v6 records remain readable and are
labelled `legacy`; missing or semantically impossible v7-v9 telemetry is
collapsed to `invalid` instead of propagating an arbitrary label. This is evidence
collection only. No output bucket affects scoring, replica choice, or load
admission.

Compare multiple correctness-qualified configurations with explicit TTFT/TPOT
SLOs using the offline Pareto reporter:

```bash
python3 bench/slo_pareto_report.py campaign.json --json > report.json
python3 bench/slo_pareto_report.py campaign.json
python3 bench/slo_pareto_report.py --print-example > campaign.example.json
```

The bounded schema-v1 manifest requires a campaign ID, expected repetition
count, named SLO pairs, and normalized cells. Every cell carries immutable
configuration/workload SHA-256 identities, an explicit
`direct_engine_crossover` or `lb_serial` domain, GPU count, observation window,
repetition, privacy-safe per-request timing/correctness/token observations, and
direct-crossover provenance where applicable. Effective tokens per step is
accepted only with a reconciled native interval.

Missing keys are schema errors. Present but missing/not-evaluated timing or
correctness makes the complete configuration ineligible and the CLI exits 3
after emitting the auditable report. Every configuration must have exactly the
declared repetitions. Direct and serial domains and workload digests form
separate cohorts, while per-GPU-hour efficiency stays comparable across GPU
allocations inside a cohort. Every cohort fixes the offered request count;
direct crossovers additionally require two configurations with balanced,
inverse engine assignments.

The frontier objective is qualified requests per GPU-hour at every supplied
SLO. Dominance uses observed repetition ranges: candidate minimum must be at
least the peer maximum for every SLO and strictly greater for one. Overlap stays
non-dominated whenever conservative all-SLO dominance cannot be established;
no confidence interval is invented. Raw
normalized cells and repetition metrics remain in JSON, and automatic
promotion is always false.

`codebench.py` emits request-level TTFT/TPOT, repetition, cache, completion, and
per-repetition observation-window data suitable for normalization, but `ok`
means only transport/output/usage/timing completeness. It is not semantic or
tool correctness. Join it to independently qualified correctness and immutable
identities; never feed it directly to the reporter or manufacture passing
correctness.

Journals contain sizes, opaque ordinals, route state, status, latency, and
aggregate usage—not prompts, generated text, request IDs, cache keys, tokens,
or upstream hostnames.
