# API tokens and per-token usage

Ramjet can mint its own client API tokens and attribute every request to the
token that made it. The engine bearer (`RJ_UPSTREAM_TOKEN`, the "master"
token) keeps working unchanged; minted tokens are an additional credential
that ramjet validates itself and then swaps for the master token before the
request reaches an engine. Engines never see a minted token.

Usage is accounted per principal (token, master, …), per model, and per
time bucket, so the dashboard can answer "who is using the box, with which
model, at what cache hit ratio and latency" by minute, hour, or day.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `RJ_API_TOKENS_MODE` | `off` | `off`: no ingress authentication (historical behaviour; requests are still accounted as `master` or `anonymous`). `on`: every proxied request must carry the master token or a valid minted token, otherwise ramjet answers `401` itself. |
| `RJ_API_STATE_DIR` | unset | Directory holding `api-tokens.json` (hashes only, mode `0600`) and `usage.sqlite3` (usage store, mode `0600`). Required when `RJ_API_TOKENS_MODE=on`; optional otherwise (usage then lives in an in-memory SQLite database). |
| `RJ_USAGE_RETENTION_DAYS` | `90` | Hourly bucket retention, 1–400. Day views are derived from hourly buckets. |
| `RJ_USAGE_MINUTE_RETENTION_HOURS` | `24` | Minute bucket retention, 1–168. |
| `RJ_USAGE_RECENT_REQUESTS` | `100000` | Individual request records kept (newest win), 0–5000000. About 250 bytes each on disk. |

`RJ_API_TOKENS_MODE=on` requires `RJ_UI_AUTH_TOKEN` (token management is a
dashboard action) and `RJ_UPSTREAM_TOKEN` (the master token that minted
tokens are exchanged for).

Credentials are read from `Authorization: Bearer <token>` or, for
Anthropic-style clients, `x-api-key: <token>`. A request carrying both must
carry the same value in both.

With `RJ_UI_AUTH_TOKEN` set, scripts may call the management API with
`Authorization: Bearer $RJ_UI_AUTH_TOKEN` instead of a browser session:

```bash
curl -s -X POST -H "Authorization: Bearer $RJ_UI_AUTH_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name":"helix-prod","description":"Helix agent fleet","expires_at_ms":null}' \
  http://127.0.0.1:8007/api/tokens
```

A fronting reverse proxy that checks the bearer itself (node06's Caddy
`@goodkey` matcher) must also admit `Bearer rjk_*` — or delegate the check to
ramjet entirely — before minted tokens can reach it.

## Persistence and scale

`api-tokens.json` is rewritten atomically (same-directory temporary file,
fsync, rename) on every mutation; a corrupt file fails startup closed rather
than silently dropping credentials. Last-use times are flushed once a minute.
At the 10,000-token cap the file is about 3 MiB.

Usage lives in `usage.sqlite3` (WAL). The request path only updates an
in-memory map of deltas since the last flush; every 10 s (and on graceful
shutdown) the deltas are merged into one row per (tier, principal, bucket,
model). A crash loses at most one interval. Memory therefore scales with the
keys active in one interval, not with history or token count, and request
volume only increments counters: a billion tokens a day costs the same as a
thousand. Disk grows with active token-hours (about 0.4–0.5 KiB per
token-minute and token-hour row, before retention).

Every request is also recorded under the reserved principal `*`, so fleet-wide
and per-model views read one row per bucket regardless of how many tokens
exist, and a single token's views read only its own rows through the primary
key. A store failure is logged and retried; it never fails a request.

## Token format

```
rjk_<12 hex id>_<64 hex secret>
```

The id is public (it is the label used in metrics and the dashboard). Only
`SHA-256(secret)` is stored. The full token is returned exactly once, by the
create call.

## Principals

Every request is attributed to one principal key:

| key | meaning |
|---|---|
| `<token id>` | a valid minted token |
| `master` | the master (engine) token |
| `anonymous` | mode `off` and the credential was neither (or absent) |
| `rejected` | mode `on` and authentication failed; the request was answered `401` by ramjet |

## HTTP API (metrics listener, `:9090`)

All `/api/tokens*` routes require a dashboard session (or
`Authorization: Bearer $RJ_UI_AUTH_TOKEN` for scripting). `/api/usage/*` is
protected the same way when `RJ_UI_AUTH_TOKEN` is set, and public like the
rest of machine view when it is not. Errors are `{"error": "message"}`.

### Token object

```json
{
  "id": "a1b2c3d4e5f6",
  "prefix": "rjk_a1b2c3d4e5f6",
  "name": "helix-prod",
  "description": "Helix agent fleet",
  "created_at_ms": 1760000000000,
  "expires_at_ms": 1770000000000,
  "revoked_at_ms": null,
  "last_used_ms": 1760000123000,
  "status": "active"
}
```

`status` is `active`, `expired`, or `revoked`. `expires_at_ms`,
`revoked_at_ms`, and `last_used_ms` may be `null`.

### Endpoints

| Method + path | Body | Response |
|---|---|---|
| `GET /api/tokens` | – | `{"enabled": bool, "tokens": [Token]}` (newest first) |
| `POST /api/tokens` | `{"name": "1-64 chars", "description": "≤512 chars, optional", "expires_at_ms": number \| null}` | `201 {"token": Token, "secret": "rjk_…"}` |
| `PATCH /api/tokens/{id}` | any of `name`, `description`, `expires_at_ms` (`null` clears expiry) | `{"token": Token}` |
| `POST /api/tokens/{id}/revoke` | – | `{"token": Token}` |
| `DELETE /api/tokens/{id}` | – | `204` (usage history is kept under the id) |

`expires_at_ms` must be in the future. At most 10,000 tokens may exist.
`GET /api/tokens/{id}` returns `{"token": Token}` for one token (`404` if unknown).

### Usage

`GET /api/usage/meta`

```json
{
  "principals": [{"key": "a1b2c3d4e5f6", "name": "helix-prod", "kind": "token", "status": "active"},
                  {"key": "master", "name": "master token", "kind": "master"}],
  "models": ["qwen3.8-flash-next"],
  "upstreams": ["http://qwen38flashnext-a:8000", "http://qwen38flashnext-b:8000"],
  "minute_retention_hours": 24,
  "hour_retention_days": 90,
  "recent_capacity": 100000,
  "tokens_enabled": true
}
```

`kind` is `token`, `deleted_token`, `master`, `anonymous`, or `rejected`;
`status` is present only for existing tokens. A deleted token's `name` is its
`rjk_<id>` prefix.
Principals include every key that has usage in retention plus every existing
token.

`GET /api/usage/series?granularity=hour&from_ms=&to_ms=&group_by=token&token=&model=&tz_offset_min=&limit=`

- `granularity`: `minute` | `hour` | `day` (default `hour`). Day buckets are
  built from hourly buckets, aligned to local midnight using
  `tz_offset_min` (minutes east of UTC, e.g. `120` for UTC+2; default `0`).
- `from_ms`/`to_ms`: default last 24h (minute: last 1h).
- `group_by`: `token` | `model` | `none` (default `token`).
- `token`, `model`: optional exact filters (`token` is a principal key).
- `limit`: with `group_by=token` and no `token` filter, how many of the
  busiest principals are returned individually (default 20, 1–500). The rest
  fold into one group with key `__other`. Its stats, quantiles included, are
  exact: everything minus the returned groups.

```json
{
  "granularity": "hour",
  "bucket_ms": 3600000,
  "from_ms": 1760000000000,
  "to_ms": 1760086400000,
  "group_by": "token",
  "group_count": 143,
  "series": [{"key": "a1b2c3d4e5f6", "points": [{"t": 1760000000000, "...": "Stats"}]}],
  "totals": [{"key": "a1b2c3d4e5f6", "...": "Stats"}],
  "overall": {"...": "Stats"}
}
```

Points are sparse: a bucket with no requests is omitted. `t` is the bucket
start. With `group_by=none`, `series`/`totals` hold one entry with key `all`.
`group_count` is the number of distinct groups with traffic in range, before
the `limit` fold. `totals` is sorted by requests, with `__other` last.

**Stats** (every field present; quantiles/ratios `null` when unknown):

```
requests, ok, client_errors, server_errors, disconnects, streamed,
prompt_tokens, cached_tokens, completion_tokens,
cache_hit_ratio,                      // cached_tokens / prompt_tokens, 0..1
cache_full, cache_partial, cache_cold, cache_unknown,   // request counts
ttft_p50_ms, ttft_p90_ms, ttft_p99_ms, ttft_mean_ms,
duration_p50_ms, duration_p90_ms, duration_p99_ms, duration_mean_ms,
decode_tps_p10, decode_tps_p50, decode_tps_p90, decode_tps_mean,
tpot_p50_ms, tpot_p90_ms,
upstream_requests: [n per upstream index],
finish_reasons: {"stop": n, "length": n, "tool_calls": n, ...}
```

Quantiles come from fixed log-spaced histograms, so they are interpolated
estimates (a few percent), not exact order statistics.

`GET /api/usage/requests?limit=200&token=&model=&errors_only=false&before_ms=`

Recent individual requests, newest first (`limit` ≤ 1000). Page backwards
with `before_ms` set to the oldest `t_ms` received. Prompts and responses are
never recorded.

```json
{
  "capacity": 100000,
  "requests": [{
    "t_ms": 1760000123000,
    "principal": "a1b2c3d4e5f6",
    "model": "qwen3.8-flash-next",
    "endpoint": "chat",
    "upstream": 1,
    "status": 200,
    "stream": true,
    "result": "complete",
    "prompt_tokens": 18234,
    "cached_tokens": 17920,
    "completion_tokens": 412,
    "cache_outcome": "partial",
    "ttft_ms": 182.4,
    "duration_ms": 5321.0,
    "decode_tps": 81.2,
    "finish_reason": "stop",
    "route_outcome": "affinity",
    "request_bytes": 81234,
    "response_bytes": 40211
  }]
}
```

`result` is `complete`, `client_disconnect`, `upstream_read_error`,
`upstream_error`, `auth_rejected`, `request_too_large`, `model_required`, or
`model_not_found`. Token counts, `ttft_ms`, `decode_tps`,
`upstream`, and `route_outcome` may be `null`.

## Prometheus

| Metric | Labels |
|---|---|
| `ramjet_principal_requests_total` | `principal`, `model`, `status_class` (`2xx`/`4xx`/`5xx`/…) |
| `ramjet_principal_tokens_total` | `principal`, `model`, `kind` (`prompt`/`cached`/`completion`) |
| `ramjet_api_token_auth_failures_total` | `reason` |
| `ramjet_api_tokens` | `status` (gauge) |

`principal` is the token id (never the secret) or one of the fixed keys
above. Model labels are capped at 32 distinct values; later ones are
reported as `other`.
