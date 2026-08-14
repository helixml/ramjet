# Configuration

mini-dynamo is configured at startup through environment variables. The normal
serving path is defaults-first: experimental tokenization, exact KV routing,
and snapshot routing are all disabled unless explicitly enabled.

## Start with the defaults

If the engine is reachable as `http://ds4-flash:8000`, no `MD_*` variable is
required. In most deployments, set only the upstream list:

```yaml
environment:
  MD_UPSTREAM: http://engine-a:8000,http://engine-b:8000
  # MD_UPSTREAM_TOKEN: ${VLLM_API_KEY} # only for protected engines
```

The proxy listens on `0.0.0.0:8000`; Prometheus metrics listen on
`0.0.0.0:9090`. Invalid settings fail startup instead of silently changing
behavior. Keep secrets in an uncommitted mode-`0600` environment file or a
secret manager.

## Router variables

### Upstreams and routing

| Variable | Default | Description |
| --- | --- | --- |
| `MD_UPSTREAM` | `http://ds4-flash:8000` | Comma-separated OpenAI-compatible engine URLs. |
| `MD_UPSTREAM_TOKEN` | unset | Bearer token used for upstream requests and probes. |
| `MD_AFFINITY` | `prefix` | `prefix` for locality/load scoring; `load` for the load-only baseline. |
| `MD_ROUTE_ALPHA` | `4` | Non-negative load penalty in the routing score. |
| `MD_ROUTE_CHUNK_BYTES` | `2048` | Bytes per approximate prefix fingerprint block. |
| `MD_ROUTE_MAX_PREFIX_BYTES` | `2097152` | Maximum request prefix bytes fingerprinted. |
| `MD_ROUTE_MAX_OVERLAP_BLOCKS` | `32` | Cap on affinity credit in fingerprint blocks. |
| `MD_ROUTE_INDEX_CAPACITY` | `100000` | Maximum entries in the approximate locality index. |
| `MD_ROUTE_LOAD_UNIT_BYTES` | `32768` | Request bytes represented by one reserved load unit. |
| `MD_ROUTE_MAX_LOAD_UNITS` | `8` | Maximum size-weighted load reservation per request. |
| `MD_ROUTE_PHASE_AWARE_LOAD` | `false` | Experimental: after the first generated token on a streaming response, reduce the request's size-weighted prefill reservation to one decode unit. |
| `MD_ROUTE_JOURNAL` | `false` | Emit privacy-bounded route start/finish records for offline replay. |
| `MD_MAX_TOKENS_STRIP` | `100000` | Strip client `max_tokens` at or above this compatibility boundary. |
| `MD_ADVERTISE_CTX_MARGIN` | `16384` | Context tokens withheld when rewriting upstream model metadata. |
| `RUST_LOG` | `info` | Standard tracing filter, for example `mini_dynamo=debug`. |

`GET /health` returns opaque replica ordinals, serving health, DSpark
reliability state, inflight work, load units, and index size. It returns `200 ok` when every replica is healthy,
`200 degraded` when at least one can serve, and `503 unhealthy` when none can
serve. Successful proxied responses include `X-Mini-Dynamo-Upstream` with an
opaque replica ordinal.

### DSpark reliability guard (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `MD_DSPARK_GUARD_MODE` | `off` | `off`, telemetry-only `observe`, or enforcing `quarantine`. |
| `MD_DSPARK_GUARD_INTERVAL_MS` | `5000` | Native engine `/metrics` polling interval, from 1–60 seconds. Missed ticks delay instead of bursting. Each request has a separate two-second timeout and 4MiB body cap. |
| `MD_DSPARK_GUARD_CONSECUTIVE_WINDOWS` | `3` | Consecutive qualifying zero-acceptance windows required, from 2 through 12. |
| `MD_DSPARK_GUARD_MIN_PROPOSED_TOKENS` | `256` | Minimum proposed draft tokens in each qualifying window. |
| `MD_DSPARK_GUARD_EXPECTED_POSITIONS` | `5` | Exact speculative positions required in every sample; use `5` for fixed K5. |
| `MD_DSPARK_GUARD_STATE_PATH` | unset | Required only for `quarantine`: normalized absolute path to a pre-created mode-0600 durable state file in a protected mode-0700 directory. |
| `MD_DSPARK_GUARD_STATE_OWNER_UID` | `0` | Required owner UID for the durable state file and directory. |
| `MD_DSPARK_GUARD_STATE_GROUP_GID` | `0` | Required group GID for the durable state file and directory. |

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
`MD_UPSTREAM_ADMISSION_MODE=compatibility` and the protected state path. Raw
incarnations and upstream URLs are never stored. The bounded schema-v1 file
also precommits a runtime-dirty marker. After an unclean LB exit or a failed
store mutation, every replica without an existing record starts fenced and its
currently attested EngineCore is durably quarantined before it can serve; an
ordinary clean LB shutdown clears the marker. The file
contains only opaque replica ordinals and SHA-256 commitments. Start with
`observe` and qualify both false-positive behavior and counter shape before
enabling enforcement.

Inspect each replica's fixed `reliability_state` and `quarantined` fields in
`/health`, plus `ds4proxy_dspark_guard_state`,
`ds4proxy_dspark_guard_windows_total`, and
`ds4proxy_dspark_guard_quarantines_total`. Durable publication failures use the
fixed `persistence_failure` state and
`ds4proxy_dspark_guard_persistence_failures_total`. Valid windows also export strict
acceptance, effective tokens per draft step, and per-position acceptance ratios
with a separate measurement-available gauge. Replica labels are opaque ordinals;
no process identity, metric payload, prompt, or completion content is exposed.

### Opaque session affinity (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `MD_SESSION_AFFINITY_MODE` | `off` | `off` or observation-only `shadow`; shadow never changes the served replica. |
| `MD_SESSION_AFFINITY_KEY` | unset | Independent 32–256-byte HMAC key required in shadow mode. |
| `MD_SESSION_AFFINITY_BONUS_BLOCKS` | `4` | Counterfactual cache-equivalent bonus, at most `MD_ROUTE_MAX_OVERLAP_BLOCKS`. |
| `MD_SESSION_AFFINITY_MAX_LOAD_DELTA` | `0` | Maximum load above the least-loaded healthy replica admitted for a counterfactual affinity target. |

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

### Tokenization (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `MD_TOKENIZER_MODE` | `off` | `off`, `remote-shadow`, or `local-shadow`; shadow modes never change the approximate decision alone. |
| `MD_TOKENIZER_PATH` | unset | `tokenizer.json` path; required by `local-shadow`. |
| `MD_TOKENIZER_SHA256` | unset | Expected 64-character artifact SHA-256; required by `local-shadow`. |
| `MD_TOKENIZER_PROFILE` | `deepseek-v4-r34` | Pinned prompt-renderer compatibility profile. |
| `MD_TOKENIZER_MIN_BYTES` | `32768` | Minimum request bytes admitted to shadow tokenization. |
| `MD_TOKENIZER_MAX_BYTES` | `2097152` | Maximum request bytes admitted to shadow tokenization. |
| `MD_TOKENIZER_WORKERS` | `1` | Bounded blocking workers for local tokenization. |
| `MD_TOKENIZER_QUEUE_CAPACITY` | `8` | Non-blocking remote-shadow queue capacity. |
| `MD_TOKENIZER_TIMEOUT_MS` | `2000` | Per-tokenization timeout. |

`remote-shadow` calls the selected engine's authenticated `/tokenize` endpoint
after request completion. `local-shadow` compares bounded local token IDs with
that remote authority in memory. Prompt text and token IDs are not retained in
logs, metrics, or journals.

### Exact route evaluation (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `MD_EXACT_ROUTE_MODE` | `off` | `off`, observation-only `shadow`, or canary `placement`. |
| `MD_EXACT_ROUTE_MANIFEST_PATH` | unset | Compatibility manifest; required when exact routing is enabled. |
| `MD_EXACT_ROUTE_MANIFEST_SHA256` | unset | Expected manifest SHA-256; required when exact routing is enabled. |
| `MD_SERVING_RUNTIME_MANIFEST_PATH` | unset | Separate serving-runtime manifest linked to the compatibility-manifest digest; required by `compatibility` admission and safe to stage while admission remains `http`. |
| `MD_SERVING_RUNTIME_MANIFEST_SHA256` | unset | Expected serving-runtime manifest SHA-256; must be configured together with its path. |
| `MD_EXACT_ROUTE_WORKERS` | `4` | Bounded exact-index lookup workers. |
| `MD_EXACT_ROUTE_TIMEOUT_MS` | `250` | Exact pre-route evaluation timeout. |
| `MD_EXACT_ROUTE_MIN_GAIN_TOKENS` | `8192` | Minimum exact cached-token gain required to move a canary request. |
| `MD_EXACT_ROUTE_MAX_LOAD_DELTA` | `0` | Maximum additional load allowed on an exact winner. |
| `MD_EXACT_ROUTE_CANARY_BPS` | `0` | Stable placement cohort size in basis points, from `0` to `10000`. Zero is instant rollback. |
| `MD_EXACT_ROUTE_CANARY_KEY` | unset | 32–256-byte HMAC key required when the placement cohort is nonzero. |
| `MD_UPSTREAM_ADMISSION_MODE` | `http` | `http` admits a replica after `/v1/models`. `compatibility` additionally requires one atomic `/v1/mini-dynamo/identity` response to match the pinned manifest. |
| `MD_UPSTREAM_ADMISSION_TIMEOUT_MS` | `5000` | Absolute timeout, at most 30 seconds, for the atomic serving-identity request. Independent of tokenization timeouts. |

Exact routing requires `MD_TOKENIZER_MODE=local-shadow`, a pinned manifest,
and exactly one inventory source: direct KV events or snapshot companions.
Placement additionally requires `MD_AFFINITY=prefix`; snapshot inventories are
shadow-only. Any timeout, attestation failure, event gap, revision change, or
missing `X-Session-ID` preserves the approximate route.

When exact placement applies, admission reservations are recomputed from the
exact warm-prefix overlap instead of the approximate block estimate that was
derived before the inventory was consulted. A request whose prefix is already
resident therefore reserves proportionally less capacity, bounded by the same
`MD_ROUTE_LOAD_UNIT_BYTES` quantum and `MD_ROUTE_MAX_LOAD_UNITS` cap. The
recompute is atomic across healthy candidates and fails closed: if any healthy
candidate lacks a trusted overlap, every original reservation is preserved. It
never changes the selected replica for the request being recomputed — placement
is decided first, and the gain/load gates still run against the pre-route
estimate. It does change the load accounting that *later* decisions read: the
reservation is what `acquire_if_healthy` adds to the upstream's load, which
becomes the next request's alpha-weighted load term. Steering warm work to an
engine that now reports lower load is the intended effect, but it is a feedback
loop, not a no-op.

This applies only to `MD_EXACT_ROUTE_MODE=placement`, and there whether or not
the exact winner actually moves the request. `shadow` stays strictly
observation-only: it never alters a reservation.

The recompute can also *raise* a reservation, up to `MD_ROUTE_MAX_LOAD_UNITS`.
If the approximate prefix index is stale and the engine has actually evicted
the prefix, exact overlap is zero and the request correctly reserves the cold
cost the approximate estimate understated. Expect `ds4proxy_upstream_load_units`
to step up on the first placement rollout; watch the upstream-split panel and
compare against the journal rather than assuming a regression.

Compatibility admission is an independent serving gate. It requires
`MD_TOKENIZER_MODE=local-shadow` plus the SHA-pinned manifest so local golden
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
`ds4proxy_upstream_compatibility_admitted`, and
`ds4proxy_upstream_admission_checks_total` before opting in.

The identity endpoint must capture the frontend and every expected EngineCore
atomically and return a bounded schema-v3 document. Each incarnation is an
opaque value of 1–256 ASCII alphanumeric/`.`/`_`/`:`/`-` bytes. mini-dynamo
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
counterfactual. `ds4proxy_exact_route_projected_balance_total` adds each
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
| `MD_KV_EVENT_MODE` | `off` | `off` or observation-only `shadow`. |
| `MD_KV_EVENT_LIVE_ENDPOINTS` | unset | Comma-separated internal `tcp://host:port` endpoints, one per upstream. |
| `MD_KV_EVENT_REPLAY_ENDPOINTS` | unset | Matching replay endpoints, one per upstream. |
| `MD_KV_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `MD_KV_EVENT_REPLAY_LIMIT` | `1024` | Maximum replay batches accepted during recovery. |
| `MD_KV_EVENT_REPLAY_TAIL_LIMIT` | `64` | Maximum bounded replay tail. |
| `MD_KV_EVENT_TIMEOUT_MS` | `5000` | Connect/replay operation deadline. |
| `MD_KV_EVENT_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `MD_KV_EVENT_RECONNECT_MAX_MS` | `10000` | Maximum reconnect backoff. |

Inventories start untrusted and fence on disconnect or invalid replay. Sparse
vLLM scheduler sequence numbers are valid; duplicate, decreasing,
out-of-range, or incomplete replay remains invalid. Raw hashes and token IDs
never enter logs, metrics, or journals.

### Snapshot inventory client (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `MD_SNAPSHOT_ROUTE_MODE` | `off` | `off` or observation-only `shadow`. |
| `MD_SNAPSHOT_ROUTE_SOCKET_PATHS` | unset | Unix socket path per upstream. |
| `MD_SNAPSHOT_ROUTE_COMPANION_UIDS` | unset | Non-root companion UID per upstream. |
| `MD_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS` | unset | Session secret path per upstream. |
| `MD_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS` | unset | Digest secret path per upstream. |
| `MD_SNAPSHOT_ROUTE_ATTESTATION_PATHS` | unset | Engine attestation path per upstream. |
| `MD_SNAPSHOT_ROUTE_GROUPS` | unset | `data_parallel_rank:group_index` per upstream. |
| `MD_SNAPSHOT_ROUTE_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `MD_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS` | `1000` | Attestation refresh interval. |
| `MD_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS` | `30000` | Absolute connection/snapshot attempt deadline. |
| `MD_SNAPSHOT_ROUTE_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `MD_SNAPSHOT_ROUTE_RECONNECT_MAX_MS` | `5000` | Maximum reconnect backoff. |

All per-upstream lists must match `MD_UPSTREAM` in length. Socket, session
secret, digest secret, and attestation paths must be normalized, absolute, and
distinct across every authority domain. Use the validated production overlay
in [`deploy/dspark_0731`](../deploy/dspark_0731/README.md); do not improvise
snapshot permissions or expose its sockets over TCP.

## Snapshot companion variables

The separate `mini-dynamo-snapshot-companion` binary is experimental and
defaults to `off`, where it needs no files, engine, or listener. Serve mode is
one companion per engine and should be configured through the repository's
validated Compose overlay.

| Variable | Default | Description |
| --- | --- | --- |
| `MD_SNAPSHOT_COMPANION_MODE` | `off` | `off` or `serve`. |
| `MD_SNAPSHOT_SOCKET_PATH` | unset | Required serve-mode Unix socket path. |
| `MD_SNAPSHOT_COMPANION_UID` | unset | Required non-root process UID. |
| `MD_SNAPSHOT_CLIENT_UID` | unset | Required, distinct non-root LB UID. |
| `MD_SNAPSHOT_SECRET_PATH` | unset | Required 32-byte session secret path. |
| `MD_SNAPSHOT_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `MD_SNAPSHOT_LIVE_ENDPOINTS` | unset | Required live KV endpoint; standalone mode accepts exactly one. |
| `MD_SNAPSHOT_REPLAY_ENDPOINTS` | unset | Matching replay endpoint. |
| `MD_SNAPSHOT_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `MD_SNAPSHOT_MAX_CLIENTS` | `2` | Active authenticated clients; standalone serve mode requires `2`. |
| `MD_SNAPSHOT_TAIL_QUEUE_CAPACITY` | `1024` | Maximum queued tail entries. |
| `MD_SNAPSHOT_TAIL_QUEUE_MAX_BYTES` | `16777216` | Maximum queued tail payload bytes. |
| `MD_SNAPSHOT_DEADLINE_MS` | `3000` | Snapshot-phase deadline. |
| `MD_SNAPSHOT_TAIL_IDLE_DEADLINE_MS` | `30000` | Tail idle/write budget. |
| `MD_SNAPSHOT_SHUTDOWN_DEADLINE_MS` | `5000` | Supervisor drain deadline. |
| `MD_SNAPSHOT_MAX_FRAME_BYTES` | `33554432` | Maximum snapshot frame bytes. |
| `MD_SNAPSHOT_MAX_TAIL_FRAME_BYTES` | `8392704` | Maximum tail frame bytes. |
| `MD_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES` | `8388608` | Maximum decoded event-batch payload; must be smaller than a tail frame. |
| `MD_SNAPSHOT_MAX_BATCH_EVENTS` | `4096` | Maximum events per decoded batch. |
| `MD_SNAPSHOT_DIGEST_SECRET_PATH` | unset | Required, distinct digest secret path. |
| `MD_SNAPSHOT_ATTESTATION_PATH` | unset | Required, distinct authenticated engine identity path. |
| `MD_SNAPSHOT_ATTESTATION_REFRESH_MS` | `1000` | Engine identity refresh interval. |
| `MD_SNAPSHOT_BLOCK_SIZE` | unset | Required engine KV block size. |
| `MD_SNAPSHOT_ATTENTION_KIND` | `mla` | `full`, `mla`, or `sink_full`. |
| `MD_SNAPSHOT_DATA_PARALLEL_RANK` | `0` | Group data-parallel rank. |
| `MD_SNAPSHOT_GROUP_INDEX` | `0` | Group index. |
| `MD_SNAPSHOT_CONNECT_TIMEOUT_MS` | `2000` | Engine KV-event connect timeout. |
| `MD_SNAPSHOT_REPLAY_TIMEOUT_MS` | `30000` | Engine replay timeout. |
| `MD_SNAPSHOT_REPLAY_LIMIT` | `10000` | Maximum replay batches. |
| `MD_SNAPSHOT_REPLAY_TAIL_LIMIT` | `1024` | Maximum replay tail batches. |
| `MD_SNAPSHOT_RECONNECT_MIN_MS` | `250` | Initial source reconnect backoff. |
| `MD_SNAPSHOT_RECONNECT_MAX_MS` | `5000` | Maximum source reconnect backoff. |
| `MD_SNAPSHOT_METRICS_BIND` | `127.0.0.1:9091` | Loopback-only TCP metrics address. |
| `MD_SNAPSHOT_METRICS_SOCKET_PATH` | unset | Metrics-only Unix socket; mutually exclusive with `MD_SNAPSHOT_METRICS_BIND`. |
| `MD_SNAPSHOT_METRICS_GROUP_GID` | unset | Required dedicated non-root group when using a metrics Unix socket. |

The metrics Unix socket must use a separate, setgid authority directory and a
group that is not the snapshot/session group. Scrapers may join only the
metrics group.

## Attestation provisioner variables

`mini-dynamo-attestation-provisioner` accepts no arguments and is silent on
success. It requires all settings below except the age override.

| Variable | Default | Description |
| --- | --- | --- |
| `MD_SNAPSHOT_ENGINE_METADATA_PATH` | required | Fresh, protected schema-v1 engine metadata input. |
| `MD_SNAPSHOT_DIGEST_SECRET_PATH` | required | Digest secret used to authenticate the output. |
| `MD_SNAPSHOT_ATTESTATION_PATH` | required | Atomically published attestation output. |
| `MD_SNAPSHOT_SECRET_OWNER_UID` | required | Exact output/input owner UID. |
| `MD_SNAPSHOT_SECRET_GROUP_GID` | required | Exact output group GID. |
| `MD_SNAPSHOT_ATTESTATION_MAX_AGE_MS` | `30000` | Maximum metadata age; bounded to five minutes. |

## Metrics and route journals

Prometheus is exposed at `/metrics`; `/metrics/upstream/{ordinal}` proxies a
single engine's metrics without exposing its address. The most useful router
families are:

- `ds4proxy_upstream_up`, inflight, and load gauges for availability.
- `ds4proxy_route_decisions_total` for route distribution.
- `ds4proxy_cache_requests_total` and prompt/cached token counters for observed
  cache outcomes.
- `ds4proxy_cache_ttft_seconds` for streaming time to first generated content.
- `ds4proxy_session_affinity_total` for bounded prospective pair, health, load,
  and score outcomes when session shadow mode is enabled.
- `ds4proxy_dspark_guard_state` and `ds4proxy_dspark_guard_windows_total` for
  DSpark degeneration observation; strict/per-position acceptance and
  effective-tokens-per-step gauges describe each valid window, while
  quarantine transitions use a separate fixed-reason counter.

With `MD_ROUTE_JOURNAL=true`, replay bounded decision snapshots without
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
before/after mini-dynamo's compatibility shim, not an engine-resolved default.
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
Journal v1-v6 records remain readable and are
labelled `legacy`; missing or semantically impossible v7/v8 telemetry is
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
