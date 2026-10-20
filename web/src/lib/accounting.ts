// API tokens + per-principal usage accounting. Types mirror the contract in
// docs/api-tokens.md exactly (snake_case JSON). In mock mode every call is
// answered by the deterministic in-memory generator in accountingMock.ts.

import { isMockMode } from "@/lib/api"
import * as mock from "@/lib/accountingMock"

export type TokenStatus = "active" | "expired" | "revoked"

export interface ApiToken {
  id: string
  prefix: string
  name: string
  description: string
  created_at_ms: number
  expires_at_ms: number | null
  revoked_at_ms: number | null
  last_used_ms: number | null
  status: TokenStatus
}

export interface TokenList {
  enabled: boolean
  tokens: ApiToken[]
}

export interface CreatedToken {
  token: ApiToken
  secret: string
}

export interface TokenCreate {
  name: string
  description?: string
  expires_at_ms: number | null
}

export type TokenPatch = Partial<{
  name: string
  description: string
  expires_at_ms: number | null
}>

export type PrincipalKind =
  | "token"
  | "deleted_token"
  | "master"
  | "anonymous"
  | "rejected"

export interface Principal {
  key: string
  name: string
  kind: PrincipalKind
  /** Present only for tokens that still exist. */
  status?: TokenStatus
}

export interface UsageMeta {
  principals: Principal[]
  models: string[]
  upstreams: string[]
  minute_retention_hours: number
  hour_retention_days: number
  recent_capacity: number
  tokens_enabled: boolean
}

export interface Stats {
  requests: number
  ok: number
  client_errors: number
  server_errors: number
  disconnects: number
  streamed: number
  prompt_tokens: number
  cached_tokens: number
  completion_tokens: number
  cache_hit_ratio: number | null
  cache_full: number
  cache_partial: number
  cache_cold: number
  cache_unknown: number
  ttft_p50_ms: number | null
  ttft_p90_ms: number | null
  ttft_p99_ms: number | null
  ttft_mean_ms: number | null
  duration_p50_ms: number | null
  duration_p90_ms: number | null
  duration_p99_ms: number | null
  duration_mean_ms: number | null
  decode_tps_p10: number | null
  decode_tps_p50: number | null
  decode_tps_p90: number | null
  decode_tps_mean: number | null
  tpot_p50_ms: number | null
  tpot_p90_ms: number | null
  upstream_requests: number[]
  finish_reasons: Record<string, number>
}

export type StatsPoint = Stats & { t: number }
export type StatsTotal = Stats & { key: string }

export type Granularity = "minute" | "hour" | "day"
export type GroupBy = "token" | "model" | "none"

export interface SeriesResponse {
  granularity: Granularity
  bucket_ms: number
  from_ms: number
  to_ms: number
  group_by: GroupBy
  /** Distinct groups with traffic in range, before the `limit` fold. */
  group_count?: number
  series: Array<{ key: string; points: StatsPoint[] }>
  totals: StatsTotal[]
  overall: Stats
}

export interface SeriesParams {
  granularity: Granularity
  from_ms: number
  to_ms: number
  group_by: GroupBy
  token?: string
  model?: string
  tz_offset_min: number
  /** group_by=token without a token filter: busiest N kept, rest in `__other`. */
  limit?: number
}

/** Server-side fold of every group beyond `limit`; its stats are exact. */
export const SERVER_OTHER_KEY = "__other"

/** Principals that are never minted tokens. */
export const FIXED_PRINCIPALS = new Set(["master", "anonymous", "rejected"])

export type RequestResult =
  | "complete"
  | "client_disconnect"
  | "upstream_read_error"
  | "upstream_error"
  | "auth_rejected"
  | "request_too_large"
  | "model_required"
  | "model_not_found"

export interface RequestRecord {
  t_ms: number
  principal: string
  model: string
  endpoint: string
  upstream: number | null
  status: number
  stream: boolean
  result: RequestResult | string
  prompt_tokens: number | null
  cached_tokens: number | null
  completion_tokens: number | null
  cache_outcome: string
  ttft_ms: number | null
  duration_ms: number
  decode_tps: number | null
  finish_reason: string | null
  route_outcome: string | null
  request_bytes: number
  response_bytes: number
}

export interface RecentRequests {
  capacity: number
  requests: RequestRecord[]
}

export interface RecentParams {
  limit: number
  token?: string
  model?: string
  errors_only?: boolean
  before_ms?: number
}

/** The dashboard session has gone; the app should show its login screen. */
export class UnauthorizedError extends Error {
  constructor(path: string) {
    super(`${path}: session expired`)
    this.name = "UnauthorizedError"
  }
}

/** The route does not exist on this LB build (or the feature is off). */
export class NotFoundError extends Error {
  constructor(path: string) {
    super(`${path}: not available on this load balancer`)
    this.name = "NotFoundError"
  }
}

async function request<T>(
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  const response = await fetch(path, {
    method: init.method ?? "GET",
    credentials: "same-origin",
    headers:
      init.body === undefined
        ? { accept: "application/json" }
        : { accept: "application/json", "content-type": "application/json" },
    body: init.body === undefined ? undefined : JSON.stringify(init.body),
  })
  if (response.status === 401) throw new UnauthorizedError(path)
  if (response.status === 404) {
    // A 404 from a known route can still carry a specific error (e.g. an
    // unknown token id); only a bare 404 means the route itself is absent.
    const payload = (await response.json().catch(() => null)) as { error?: string } | null
    if (payload?.error) throw new Error(payload.error)
    throw new NotFoundError(path)
  }
  if (!response.ok) {
    const payload = (await response.json().catch(() => null)) as { error?: string } | null
    throw new Error(payload?.error ?? `${path}: HTTP ${response.status}`)
  }
  if (response.status === 204) return undefined as T
  return (await response.json()) as T
}

function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === "") continue
    search.set(key, String(value))
  }
  const text = search.toString()
  return text ? `?${text}` : ""
}

export function listTokens(): Promise<TokenList> {
  if (isMockMode()) return mock.listTokens()
  return request<TokenList>("/api/tokens")
}

export async function getToken(id: string): Promise<ApiToken> {
  if (isMockMode()) return mock.getToken(id)
  const result = await request<{ token: ApiToken }>(`/api/tokens/${encodeURIComponent(id)}`)
  return result.token
}

export function createToken(body: TokenCreate): Promise<CreatedToken> {
  if (isMockMode()) return mock.createToken(body)
  return request<CreatedToken>("/api/tokens", { method: "POST", body })
}

export async function updateToken(id: string, patch: TokenPatch): Promise<ApiToken> {
  if (isMockMode()) return mock.updateToken(id, patch)
  const result = await request<{ token: ApiToken }>(
    `/api/tokens/${encodeURIComponent(id)}`,
    { method: "PATCH", body: patch },
  )
  return result.token
}

export async function revokeToken(id: string): Promise<ApiToken> {
  if (isMockMode()) return mock.revokeToken(id)
  const result = await request<{ token: ApiToken }>(
    `/api/tokens/${encodeURIComponent(id)}/revoke`,
    { method: "POST" },
  )
  return result.token
}

export function deleteToken(id: string): Promise<void> {
  if (isMockMode()) return mock.deleteToken(id)
  return request<void>(`/api/tokens/${encodeURIComponent(id)}`, { method: "DELETE" })
}

/**
 * Token names are free text, so two tokens may share one. Suffix duplicates
 * with their id prefix so legends, filters, and tables stay unambiguous.
 */
function disambiguate(meta: UsageMeta): UsageMeta {
  const counts = new Map<string, number>()
  for (const principal of meta.principals) {
    counts.set(principal.name, (counts.get(principal.name) ?? 0) + 1)
  }
  return {
    ...meta,
    principals: meta.principals.map((principal) =>
      (counts.get(principal.name) ?? 0) > 1
        ? { ...principal, name: `${principal.name} · ${principal.key.slice(0, 6)}` }
        : principal,
    ),
  }
}

export async function fetchUsageMeta(): Promise<UsageMeta> {
  if (isMockMode()) return disambiguate(await mock.usageMeta())
  return disambiguate(await request<UsageMeta>("/api/usage/meta"))
}

export function fetchUsageSeries(params: SeriesParams): Promise<SeriesResponse> {
  if (isMockMode()) return mock.usageSeries(params)
  return request<SeriesResponse>(
    `/api/usage/series${query({
      granularity: params.granularity,
      from_ms: Math.round(params.from_ms),
      to_ms: Math.round(params.to_ms),
      group_by: params.group_by,
      token: params.token,
      model: params.model,
      tz_offset_min: params.tz_offset_min,
      limit: params.limit,
    })}`,
  )
}

export function fetchRecentRequests(params: RecentParams): Promise<RecentRequests> {
  if (isMockMode()) return mock.recentRequests(params)
  return request<RecentRequests>(
    `/api/usage/requests${query({
      limit: params.limit,
      token: params.token,
      model: params.model,
      errors_only: params.errors_only ? true : undefined,
      before_ms: params.before_ms,
    })}`,
  )
}

// ---------------------------------------------------------------- helpers

export const BUCKET_MS: Record<Granularity, number> = {
  minute: 60_000,
  hour: 3_600_000,
  day: 86_400_000,
}

/** Minutes east of UTC, the backend's `tz_offset_min`. */
export function localTzOffsetMin(): number {
  return -new Date().getTimezoneOffset()
}

/**
 * Start of the bucket containing `t`. Day buckets align to local midnight
 * through the same fixed offset the backend uses; minute/hour are UTC.
 */
export function bucketFloor(t: number, granularity: Granularity, tzOffsetMin: number): number {
  const size = BUCKET_MS[granularity]
  const shift = granularity === "day" ? tzOffsetMin * 60_000 : 0
  return Math.floor((t + shift) / size) * size - shift
}

export function emptyStats(upstreams = 0): Stats {
  return {
    requests: 0,
    ok: 0,
    client_errors: 0,
    server_errors: 0,
    disconnects: 0,
    streamed: 0,
    prompt_tokens: 0,
    cached_tokens: 0,
    completion_tokens: 0,
    cache_hit_ratio: null,
    cache_full: 0,
    cache_partial: 0,
    cache_cold: 0,
    cache_unknown: 0,
    ttft_p50_ms: null,
    ttft_p90_ms: null,
    ttft_p99_ms: null,
    ttft_mean_ms: null,
    duration_p50_ms: null,
    duration_p90_ms: null,
    duration_p99_ms: null,
    duration_mean_ms: null,
    decode_tps_p10: null,
    decode_tps_p50: null,
    decode_tps_p90: null,
    decode_tps_mean: null,
    tpot_p50_ms: null,
    tpot_p90_ms: null,
    upstream_requests: Array.from({ length: upstreams }, () => 0),
    finish_reasons: {},
  }
}

/**
 * Every bucket start in [from, to), plus any point that lands off-grid, so a
 * sparse server series can be plotted as a dense one. Missing buckets are
 * "zero requests", which is different from an unknown quantile.
 */
export function bucketGrid(
  fromMs: number,
  toMs: number,
  granularity: Granularity,
  tzOffsetMin: number,
  extra: Iterable<number> = [],
): number[] {
  const size = BUCKET_MS[granularity]
  const grid = new Set<number>()
  const first = bucketFloor(fromMs, granularity, tzOffsetMin)
  // Bound the loop so a malformed response cannot hang the tab.
  for (let t = first, n = 0; t < toMs && n < 20_000; t += size, n += 1) grid.add(t)
  for (const t of extra) grid.add(t)
  return [...grid].sort((a, b) => a - b)
}

/** Share of requests that were not OK (4xx/5xx), 0..1, or null when idle. */
export function errorRate(stats: Pick<Stats, "requests" | "client_errors" | "server_errors">): number | null {
  if (stats.requests <= 0) return null
  return (stats.client_errors + stats.server_errors) / stats.requests
}

export function tokenStatusOf(token: ApiToken, now: number): TokenStatus {
  if (token.revoked_at_ms != null) return "revoked"
  if (token.expires_at_ms != null && token.expires_at_ms <= now) return "expired"
  return token.status
}
