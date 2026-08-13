# Configuration

mini-dynamo is configured at startup through environment variables. The normal
serving path is defaults-first: experimental tokenization, exact KV routing,
and snapshot routing are all disabled unless explicitly enabled.

## Start with the defaults

If the engine is reachable as `http://ds4-flash:8000`, no `DS4_*` variable is
required. In most deployments, set only the upstream list:

```yaml
environment:
  DS4_UPSTREAM: http://engine-a:8000,http://engine-b:8000
  # DS4_UPSTREAM_TOKEN: ${VLLM_API_KEY} # only for protected engines
```

The proxy listens on `0.0.0.0:8000`; Prometheus metrics listen on
`0.0.0.0:9090`. Invalid settings fail startup instead of silently changing
behavior. Keep secrets in an uncommitted mode-`0600` environment file or a
secret manager.

## Router variables

### Upstreams and routing

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_UPSTREAM` | `http://ds4-flash:8000` | Comma-separated OpenAI-compatible engine URLs. |
| `DS4_UPSTREAM_TOKEN` | unset | Bearer token used for upstream requests and probes. |
| `DS4_AFFINITY` | `prefix` | `prefix` for locality/load scoring; `load` for the load-only baseline. |
| `DS4_ROUTE_ALPHA` | `4` | Non-negative load penalty in the routing score. |
| `DS4_ROUTE_CHUNK_BYTES` | `2048` | Bytes per approximate prefix fingerprint block. |
| `DS4_ROUTE_MAX_PREFIX_BYTES` | `2097152` | Maximum request prefix bytes fingerprinted. |
| `DS4_ROUTE_MAX_OVERLAP_BLOCKS` | `32` | Cap on affinity credit in fingerprint blocks. |
| `DS4_ROUTE_INDEX_CAPACITY` | `100000` | Maximum entries in the approximate locality index. |
| `DS4_ROUTE_LOAD_UNIT_BYTES` | `32768` | Request bytes represented by one reserved load unit. |
| `DS4_ROUTE_MAX_LOAD_UNITS` | `8` | Maximum size-weighted load reservation per request. |
| `DS4_ROUTE_JOURNAL` | `false` | Emit privacy-bounded route start/finish records for offline replay. |
| `DS4_MAX_TOKENS_STRIP` | `100000` | Strip client `max_tokens` at or above this compatibility boundary. |
| `DS4_ADVERTISE_CTX_MARGIN` | `16384` | Context tokens withheld when rewriting upstream model metadata. |
| `RUST_LOG` | `info` | Standard tracing filter, for example `mini_dynamo=debug`. |

`GET /health` returns opaque replica ordinals, serving health, inflight work,
load units, and index size. It returns `200 ok` when every replica is healthy,
`200 degraded` when at least one can serve, and `503 unhealthy` when none can
serve. Successful proxied responses include `X-Mini-Dynamo-Upstream` with an
opaque replica ordinal.

### Tokenization (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_TOKENIZER_MODE` | `off` | `off`, `remote-shadow`, or `local-shadow`; shadow modes never change the approximate decision alone. |
| `DS4_TOKENIZER_PATH` | unset | `tokenizer.json` path; required by `local-shadow`. |
| `DS4_TOKENIZER_SHA256` | unset | Expected 64-character artifact SHA-256; required by `local-shadow`. |
| `DS4_TOKENIZER_PROFILE` | `deepseek-v4-r34` | Pinned prompt-renderer compatibility profile. |
| `DS4_TOKENIZER_MIN_BYTES` | `32768` | Minimum request bytes admitted to shadow tokenization. |
| `DS4_TOKENIZER_MAX_BYTES` | `2097152` | Maximum request bytes admitted to shadow tokenization. |
| `DS4_TOKENIZER_WORKERS` | `1` | Bounded blocking workers for local tokenization. |
| `DS4_TOKENIZER_QUEUE_CAPACITY` | `8` | Non-blocking remote-shadow queue capacity. |
| `DS4_TOKENIZER_TIMEOUT_MS` | `2000` | Per-tokenization timeout. |

`remote-shadow` calls the selected engine's authenticated `/tokenize` endpoint
after request completion. `local-shadow` compares bounded local token IDs with
that remote authority in memory. Prompt text and token IDs are not retained in
logs, metrics, or journals.

### Exact route evaluation (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_EXACT_ROUTE_MODE` | `off` | `off`, observation-only `shadow`, or canary `placement`. |
| `DS4_EXACT_ROUTE_MANIFEST_PATH` | unset | Compatibility manifest; required when exact routing is enabled. |
| `DS4_EXACT_ROUTE_MANIFEST_SHA256` | unset | Expected manifest SHA-256; required when exact routing is enabled. |
| `DS4_EXACT_ROUTE_WORKERS` | `4` | Bounded exact-index lookup workers. |
| `DS4_EXACT_ROUTE_TIMEOUT_MS` | `250` | Exact pre-route evaluation timeout. |
| `DS4_EXACT_ROUTE_MIN_GAIN_TOKENS` | `8192` | Minimum exact cached-token gain required to move a canary request. |
| `DS4_EXACT_ROUTE_MAX_LOAD_DELTA` | `0` | Maximum additional load allowed on an exact winner. |
| `DS4_EXACT_ROUTE_CANARY_BPS` | `0` | Stable placement cohort size in basis points, from `0` to `10000`. Zero is instant rollback. |
| `DS4_EXACT_ROUTE_CANARY_KEY` | unset | 32–256-byte HMAC key required when the placement cohort is nonzero. |

Exact routing requires `DS4_TOKENIZER_MODE=local-shadow`, a pinned manifest,
and exactly one inventory source: direct KV events or snapshot companions.
Placement additionally requires `DS4_AFFINITY=prefix`; snapshot inventories are
shadow-only. Any timeout, attestation failure, event gap, revision change, or
missing `X-Session-ID` preserves the approximate route.

### Direct KV-event inventory (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_KV_EVENT_MODE` | `off` | `off` or observation-only `shadow`. |
| `DS4_KV_EVENT_LIVE_ENDPOINTS` | unset | Comma-separated internal `tcp://host:port` endpoints, one per upstream. |
| `DS4_KV_EVENT_REPLAY_ENDPOINTS` | unset | Matching replay endpoints, one per upstream. |
| `DS4_KV_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `DS4_KV_EVENT_REPLAY_LIMIT` | `1024` | Maximum replay batches accepted during recovery. |
| `DS4_KV_EVENT_REPLAY_TAIL_LIMIT` | `64` | Maximum bounded replay tail. |
| `DS4_KV_EVENT_TIMEOUT_MS` | `5000` | Connect/replay operation deadline. |
| `DS4_KV_EVENT_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `DS4_KV_EVENT_RECONNECT_MAX_MS` | `10000` | Maximum reconnect backoff. |

Inventories start untrusted and fence on disconnect or invalid replay. Sparse
vLLM scheduler sequence numbers are valid; duplicate, decreasing,
out-of-range, or incomplete replay remains invalid. Raw hashes and token IDs
never enter logs, metrics, or journals.

### Snapshot inventory client (experimental)

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_SNAPSHOT_ROUTE_MODE` | `off` | `off` or observation-only `shadow`. |
| `DS4_SNAPSHOT_ROUTE_SOCKET_PATHS` | unset | Unix socket path per upstream. |
| `DS4_SNAPSHOT_ROUTE_COMPANION_UIDS` | unset | Non-root companion UID per upstream. |
| `DS4_SNAPSHOT_ROUTE_SESSION_SECRET_PATHS` | unset | Session secret path per upstream. |
| `DS4_SNAPSHOT_ROUTE_DIGEST_SECRET_PATHS` | unset | Digest secret path per upstream. |
| `DS4_SNAPSHOT_ROUTE_ATTESTATION_PATHS` | unset | Engine attestation path per upstream. |
| `DS4_SNAPSHOT_ROUTE_GROUPS` | unset | `data_parallel_rank:group_index` per upstream. |
| `DS4_SNAPSHOT_ROUTE_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `DS4_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS` | `1000` | Attestation refresh interval. |
| `DS4_SNAPSHOT_ROUTE_ATTEMPT_TIMEOUT_MS` | `30000` | Absolute connection/snapshot attempt deadline. |
| `DS4_SNAPSHOT_ROUTE_RECONNECT_MIN_MS` | `250` | Initial reconnect backoff. |
| `DS4_SNAPSHOT_ROUTE_RECONNECT_MAX_MS` | `5000` | Maximum reconnect backoff. |

All per-upstream lists must match `DS4_UPSTREAM` in length. Socket, session
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
| `DS4_SNAPSHOT_COMPANION_MODE` | `off` | `off` or `serve`. |
| `DS4_SNAPSHOT_SOCKET_PATH` | unset | Required serve-mode Unix socket path. |
| `DS4_SNAPSHOT_COMPANION_UID` | unset | Required non-root process UID. |
| `DS4_SNAPSHOT_CLIENT_UID` | unset | Required, distinct non-root LB UID. |
| `DS4_SNAPSHOT_SECRET_PATH` | unset | Required 32-byte session secret path. |
| `DS4_SNAPSHOT_SECRET_OWNER_UID` | `0` | Expected owner of protected inputs. |
| `DS4_SNAPSHOT_LIVE_ENDPOINTS` | unset | Required live KV endpoint; standalone mode accepts exactly one. |
| `DS4_SNAPSHOT_REPLAY_ENDPOINTS` | unset | Matching replay endpoint. |
| `DS4_SNAPSHOT_EVENT_TOPIC` | empty | Optional ZMQ topic, at most 256 bytes. |
| `DS4_SNAPSHOT_MAX_CLIENTS` | `2` | Active authenticated clients; standalone serve mode requires `2`. |
| `DS4_SNAPSHOT_TAIL_QUEUE_CAPACITY` | `1024` | Maximum queued tail entries. |
| `DS4_SNAPSHOT_TAIL_QUEUE_MAX_BYTES` | `16777216` | Maximum queued tail payload bytes. |
| `DS4_SNAPSHOT_DEADLINE_MS` | `3000` | Snapshot-phase deadline. |
| `DS4_SNAPSHOT_TAIL_IDLE_DEADLINE_MS` | `30000` | Tail idle/write budget. |
| `DS4_SNAPSHOT_SHUTDOWN_DEADLINE_MS` | `5000` | Supervisor drain deadline. |
| `DS4_SNAPSHOT_MAX_FRAME_BYTES` | `33554432` | Maximum snapshot frame bytes. |
| `DS4_SNAPSHOT_MAX_TAIL_FRAME_BYTES` | `8392704` | Maximum tail frame bytes. |
| `DS4_SNAPSHOT_MAX_BATCH_PAYLOAD_BYTES` | `8388608` | Maximum decoded event-batch payload; must be smaller than a tail frame. |
| `DS4_SNAPSHOT_MAX_BATCH_EVENTS` | `4096` | Maximum events per decoded batch. |
| `DS4_SNAPSHOT_DIGEST_SECRET_PATH` | unset | Required, distinct digest secret path. |
| `DS4_SNAPSHOT_ATTESTATION_PATH` | unset | Required, distinct authenticated engine identity path. |
| `DS4_SNAPSHOT_ATTESTATION_REFRESH_MS` | `1000` | Engine identity refresh interval. |
| `DS4_SNAPSHOT_BLOCK_SIZE` | unset | Required engine KV block size. |
| `DS4_SNAPSHOT_ATTENTION_KIND` | `mla` | `full`, `mla`, or `sink_full`. |
| `DS4_SNAPSHOT_DATA_PARALLEL_RANK` | `0` | Group data-parallel rank. |
| `DS4_SNAPSHOT_GROUP_INDEX` | `0` | Group index. |
| `DS4_SNAPSHOT_CONNECT_TIMEOUT_MS` | `2000` | Engine KV-event connect timeout. |
| `DS4_SNAPSHOT_REPLAY_TIMEOUT_MS` | `30000` | Engine replay timeout. |
| `DS4_SNAPSHOT_REPLAY_LIMIT` | `10000` | Maximum replay batches. |
| `DS4_SNAPSHOT_REPLAY_TAIL_LIMIT` | `1024` | Maximum replay tail batches. |
| `DS4_SNAPSHOT_RECONNECT_MIN_MS` | `250` | Initial source reconnect backoff. |
| `DS4_SNAPSHOT_RECONNECT_MAX_MS` | `5000` | Maximum source reconnect backoff. |
| `DS4_SNAPSHOT_METRICS_BIND` | `127.0.0.1:9091` | Loopback-only TCP metrics address. |
| `DS4_SNAPSHOT_METRICS_SOCKET_PATH` | unset | Metrics-only Unix socket; mutually exclusive with `DS4_SNAPSHOT_METRICS_BIND`. |
| `DS4_SNAPSHOT_METRICS_GROUP_GID` | unset | Required dedicated non-root group when using a metrics Unix socket. |

The metrics Unix socket must use a separate, setgid authority directory and a
group that is not the snapshot/session group. Scrapers may join only the
metrics group.

## Attestation provisioner variables

`mini-dynamo-attestation-provisioner` accepts no arguments and is silent on
success. It requires all settings below except the age override.

| Variable | Default | Description |
| --- | --- | --- |
| `DS4_SNAPSHOT_ENGINE_METADATA_PATH` | required | Fresh, protected schema-v1 engine metadata input. |
| `DS4_SNAPSHOT_DIGEST_SECRET_PATH` | required | Digest secret used to authenticate the output. |
| `DS4_SNAPSHOT_ATTESTATION_PATH` | required | Atomically published attestation output. |
| `DS4_SNAPSHOT_SECRET_OWNER_UID` | required | Exact output/input owner UID. |
| `DS4_SNAPSHOT_SECRET_GROUP_GID` | required | Exact output group GID. |
| `DS4_SNAPSHOT_ATTESTATION_MAX_AGE_MS` | `30000` | Maximum metadata age; bounded to five minutes. |

## Metrics and route journals

Prometheus is exposed at `/metrics`; `/metrics/upstream/{ordinal}` proxies a
single engine's metrics without exposing its address. The most useful router
families are:

- `ds4proxy_upstream_up`, inflight, and load gauges for availability.
- `ds4proxy_route_decisions_total` for route distribution.
- `ds4proxy_cache_requests_total` and prompt/cached token counters for observed
  cache outcomes.
- `ds4proxy_cache_ttft_seconds` for streaming time to first generated content.

With `DS4_ROUTE_JOURNAL=true`, replay bounded decision snapshots without
changing live traffic:

```bash
docker logs ds4-loadbalancer 2>&1 | \
  python3 bench/route_replay.py - --alphas 1,2,4,8 --caps 8,16,32,64
```

Journals contain sizes, opaque ordinals, route state, status, latency, and
aggregate usage—not prompts, generated text, request IDs, cache keys, tokens,
or upstream hostnames.
