// Deterministic synthetic tokens + usage for `?mock=1`. Usage is a pure
// function of (principal, model, bucket start), so every granularity and
// group-by agrees with every other and a reload draws the same picture.
// Token create/edit/revoke/delete mutate an in-memory list only.

import {
  BUCKET_MS,
  bucketFloor,
  emptyStats,
  tokenStatusOf,
  type ApiToken,
  type CreatedToken,
  type Principal,
  type RecentParams,
  type RecentRequests,
  type RequestRecord,
  type SeriesParams,
  type SeriesResponse,
  type Stats,
  type StatsPoint,
  type TokenCreate,
  type TokenList,
  type TokenPatch,
  type UsageMeta,
} from "@/lib/accounting"

const HOUR = 3_600_000
const DAY = 86_400_000
const NOW0 = Date.now()

const MODELS = ["qwen3.8-flash-next", "glm-5.3"]
const UPSTREAMS = [
  "http://qwen38flashnext-a:8000",
  "http://qwen38flashnext-b:8000",
  "http://glm53-a:30000",
]
/** Upstream indices that serve each model. */
const MODEL_UPSTREAMS: Record<string, number[]> = {
  "qwen3.8-flash-next": [0, 1],
  "glm-5.3": [2],
}
const MODEL_TPS: Record<string, number> = {
  "qwen3.8-flash-next": 86,
  "glm-5.3": 47,
}

interface Profile {
  /** Requests per hour at the diurnal peak, per model. */
  rate: Record<string, number>
  promptAvg: number
  completionAvg: number
  cache: number
  clientErr: number
  serverErr: number
  /** Hour-of-day (local) shape: "office", "nightly", or "flat". */
  shape: "office" | "nightly" | "flat"
  /** Usage exists only inside [from, until). */
  from: number
  until: number
  rejected?: boolean
}

function hex(seed: number, length: number): string {
  const random = prng(seed)
  let out = ""
  while (out.length < length) out += Math.floor(random() * 16).toString(16)
  return out
}

function makeToken(
  id: string,
  name: string,
  description: string,
  createdAgo: number,
  expiresIn: number | null,
  revokedAgo: number | null,
): ApiToken {
  return {
    id,
    prefix: `rjk_${id}`,
    name,
    description,
    created_at_ms: NOW0 - createdAgo,
    expires_at_ms: expiresIn == null ? null : NOW0 + expiresIn,
    revoked_at_ms: revokedAgo == null ? null : NOW0 - revokedAgo,
    last_used_ms: null,
    status: "active",
  }
}

let tokens: ApiToken[] = [
  makeToken("a1b2c3d4e5f6", "helix-prod", "Helix agent fleet (production org)", 120 * DAY, null, null),
  makeToken("7c41e09ab2d3", "ci-evals", "Nightly agentbench + reasoning matrix", 40 * DAY, 5 * DAY, null),
  makeToken("3f9a5be81c07", "notebook-karolis", "Ad-hoc notebook experiments", 30 * DAY, 60 * DAY, 6 * DAY),
  makeToken("e52d6a90f418", "contractor-trial", "Two-week evaluation for an external team", 25 * DAY, -11 * DAY, null),
]

const DELETED_ID = "9b0c7d1e2f3a"

const profiles: Record<string, Profile> = {
  a1b2c3d4e5f6: {
    rate: { "qwen3.8-flash-next": 1400, "glm-5.3": 260 },
    promptAvg: 21_000,
    completionAvg: 380,
    cache: 0.91,
    clientErr: 0.004,
    serverErr: 0.003,
    shape: "office",
    from: NOW0 - 120 * DAY,
    until: Infinity,
  },
  "7c41e09ab2d3": {
    rate: { "qwen3.8-flash-next": 900, "glm-5.3": 520 },
    promptAvg: 9_000,
    completionAvg: 640,
    cache: 0.62,
    clientErr: 0.012,
    serverErr: 0.006,
    shape: "nightly",
    from: NOW0 - 40 * DAY,
    until: Infinity,
  },
  "3f9a5be81c07": {
    rate: { "qwen3.8-flash-next": 90, "glm-5.3": 40 },
    promptAvg: 4_200,
    completionAvg: 900,
    cache: 0.35,
    clientErr: 0.02,
    serverErr: 0.004,
    shape: "office",
    from: NOW0 - 30 * DAY,
    until: NOW0 - 6 * DAY,
  },
  e52d6a90f418: {
    rate: { "qwen3.8-flash-next": 220, "glm-5.3": 0 },
    promptAvg: 12_000,
    completionAvg: 300,
    cache: 0.74,
    clientErr: 0.01,
    serverErr: 0.004,
    shape: "office",
    from: NOW0 - 25 * DAY,
    until: NOW0 - 11 * DAY,
  },
  [DELETED_ID]: {
    rate: { "qwen3.8-flash-next": 160, "glm-5.3": 0 },
    promptAvg: 7_000,
    completionAvg: 250,
    cache: 0.55,
    clientErr: 0.01,
    serverErr: 0.004,
    shape: "flat",
    from: NOW0 - 80 * DAY,
    until: NOW0 - 50 * DAY,
  },
  master: {
    rate: { "qwen3.8-flash-next": 300, "glm-5.3": 60 },
    promptAvg: 16_000,
    completionAvg: 420,
    cache: 0.83,
    clientErr: 0.006,
    serverErr: 0.004,
    shape: "flat",
    from: NOW0 - 90 * DAY,
    until: Infinity,
  },
  anonymous: {
    rate: { "qwen3.8-flash-next": 120, "glm-5.3": 0 },
    promptAvg: 6_000,
    completionAvg: 200,
    cache: 0.5,
    clientErr: 0.01,
    serverErr: 0.004,
    shape: "office",
    from: NOW0 - 90 * DAY,
    until: NOW0 - 21 * DAY,
  },
  rejected: {
    rate: { "qwen3.8-flash-next": 14, "glm-5.3": 0 },
    promptAvg: 0,
    completionAvg: 0,
    cache: 0,
    clientErr: 1,
    serverErr: 0,
    shape: "flat",
    from: NOW0 - 21 * DAY,
    until: Infinity,
    rejected: true,
  },
}

// A long tail of smaller tokens, so search, paging, and the server-side
// `__other` fold all have something to do.
const TEAMS = [
  "search", "billing", "support-bot", "docs-qa", "code-review", "triage",
  "sales-assist", "eval-harness", "data-pipe", "ops-runbook", "translate",
  "summarizer", "rag-index", "agent-lab",
]
const STAGES = ["prod", "staging", "dev", "batch"]
const PURPOSES = [
  "Customer-facing assistant",
  "Internal tooling",
  "Offline batch jobs",
  "Experiment, may be deleted",
  "Load test client",
]
for (let i = 0; i < 56; i++) {
  const random = prng(9000 + i)
  const id = hex(7000 + i, 12)
  const name = `${TEAMS[i % TEAMS.length]}-${STAGES[Math.floor(i / TEAMS.length) % STAGES.length]}`
  const createdAgo = (5 + random() * 110) * DAY
  const roll = random()
  const revokedAgo = roll < 0.1 ? random() * createdAgo * 0.5 : null
  const expiresIn = roll > 0.1 && roll < 0.18 ? -random() * 20 * DAY : roll > 0.6 ? (2 + random() * 80) * DAY : null
  tokens.push(
    makeToken(id, name, `${PURPOSES[i % PURPOSES.length]} · ${TEAMS[i % TEAMS.length]}`, createdAgo, expiresIn, revokedAgo),
  )
  const quiet = random() < 0.15 // minted but never used
  profiles[id] = {
    rate: {
      "qwen3.8-flash-next": quiet ? 0 : 4 + random() * random() * 260,
      "glm-5.3": quiet || random() < 0.5 ? 0 : 2 + random() * 60,
    },
    promptAvg: 2_000 + random() * 24_000,
    completionAvg: 120 + random() * 900,
    cache: 0.3 + random() * 0.65,
    clientErr: 0.002 + random() * 0.02,
    serverErr: 0.001 + random() * 0.006,
    shape: random() < 0.6 ? "office" : "flat",
    from: NOW0 - createdAgo,
    until: Infinity,
  }
}

let byId = new Map(tokens.map((token) => [token.id, token]))
function reindex() {
  byId = new Map(tokens.map((token) => [token.id, token]))
}

/** Valid tokens stop producing traffic once revoked or expired. */
function activeWindow(key: string): [number, number] {
  const profile = profiles[key]
  const token = byId.get(key)
  let until = profile.until
  if (token?.revoked_at_ms != null) until = Math.min(until, token.revoked_at_ms)
  if (token?.expires_at_ms != null) until = Math.min(until, token.expires_at_ms)
  return [profile.from, until]
}

// ---------------------------------------------------------------- random

function prng(seed: number): () => number {
  let state = seed >>> 0
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0
    return state / 0xffffffff
  }
}

function hashString(text: string): number {
  let h = 2166136261
  for (let i = 0; i < text.length; i++) {
    h ^= text.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  return h >>> 0
}

/** Uniform [0,1) from a handful of integers, stable across calls. */
function noise(...parts: number[]): number {
  let h = 0x9e3779b9
  for (const part of parts) {
    h ^= Math.floor(part) | 0
    h = Math.imul(h ^ (h >>> 16), 0x85ebca6b)
    h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35)
    h ^= h >>> 16
    // Mix the high half of large timestamps as well.
    h ^= Math.floor(part / 4294967296) | 0
  }
  return (h >>> 0) / 4294967296
}

function diurnal(shape: Profile["shape"], t: number): number {
  const local = new Date(t)
  const hour = local.getHours() + local.getMinutes() / 60
  const weekday = local.getDay()
  if (shape === "nightly") {
    // A batch window around 02:00–04:30 plus a little daytime poking.
    return hour >= 2 && hour < 4.5 ? 1 : 0.04
  }
  const office = Math.max(0, Math.sin(((hour - 7) / 24) * Math.PI * 2)) * 0.85 + 0.15
  const weekend = weekday === 0 || weekday === 6 ? 0.35 : 1
  return shape === "office" ? office * weekend : 0.55 + office * 0.45
}

// ---------------------------------------------------------------- stats

interface Acc {
  stats: Stats
  w: Record<string, number>
}

const QUANTILE_KEYS = [
  "ttft_p50_ms",
  "ttft_p90_ms",
  "ttft_p99_ms",
  "ttft_mean_ms",
  "duration_p50_ms",
  "duration_p90_ms",
  "duration_p99_ms",
  "duration_mean_ms",
  "decode_tps_p10",
  "decode_tps_p50",
  "decode_tps_p90",
  "decode_tps_mean",
  "tpot_p50_ms",
  "tpot_p90_ms",
] as const

const COUNT_KEYS = [
  "requests",
  "ok",
  "client_errors",
  "server_errors",
  "disconnects",
  "streamed",
  "prompt_tokens",
  "cached_tokens",
  "completion_tokens",
  "cache_full",
  "cache_partial",
  "cache_cold",
  "cache_unknown",
] as const

function newAcc(): Acc {
  return { stats: emptyStats(UPSTREAMS.length), w: {} }
}

/** Mock-only merge: counts sum, quantiles are request-weighted means. */
function addTo(acc: Acc, stats: Stats) {
  for (const key of COUNT_KEYS) acc.stats[key] += stats[key]
  for (const key of QUANTILE_KEYS) {
    const value = stats[key]
    if (value == null) continue
    const weight = Math.max(1, stats.ok)
    acc.w[key] = (acc.w[key] ?? 0) + weight
    acc.w[`${key}:sum`] = (acc.w[`${key}:sum`] ?? 0) + value * weight
  }
  stats.upstream_requests.forEach((n, index) => {
    acc.stats.upstream_requests[index] = (acc.stats.upstream_requests[index] ?? 0) + n
  })
  for (const [reason, n] of Object.entries(stats.finish_reasons)) {
    acc.stats.finish_reasons[reason] = (acc.stats.finish_reasons[reason] ?? 0) + n
  }
}

/** Merge one accumulator into another (sums and weighted quantile sums). */
function mergeAcc(into: Acc, from: Acc) {
  for (const key of COUNT_KEYS) into.stats[key] += from.stats[key]
  for (const [key, value] of Object.entries(from.w)) into.w[key] = (into.w[key] ?? 0) + value
  from.stats.upstream_requests.forEach((n, index) => {
    into.stats.upstream_requests[index] = (into.stats.upstream_requests[index] ?? 0) + n
  })
  for (const [reason, n] of Object.entries(from.stats.finish_reasons)) {
    into.stats.finish_reasons[reason] = (into.stats.finish_reasons[reason] ?? 0) + n
  }
}

function finish(acc: Acc): Stats {
  const out: Stats = {
    ...acc.stats,
    upstream_requests: [...acc.stats.upstream_requests],
    finish_reasons: { ...acc.stats.finish_reasons },
  }
  for (const key of QUANTILE_KEYS) {
    const weight = acc.w[key]
    out[key] = weight ? acc.w[`${key}:sum`] / weight : null
  }
  out.cache_hit_ratio = out.prompt_tokens > 0 ? out.cached_tokens / out.prompt_tokens : null
  return out
}

/** One principal × model × bucket, or null for an idle bucket. */
function bucketStats(key: string, model: string, t: number, bucketMs: number): Stats | null {
  const profile = profiles[key]
  const peak = profile.rate[model] ?? 0
  if (peak <= 0) return null
  const [from, until] = activeWindow(key)
  const end = Math.min(t + bucketMs, Date.now())
  if (end <= from || t >= until) return null
  const seed = hashString(`${key}/${model}`)
  const r = (salt: number) => noise(seed, t / 1000, salt)
  const lambda = peak * diurnal(profile.shape, t + bucketMs / 2) * (bucketMs / HOUR)
  let requests: number
  if (lambda < 1) {
    requests = r(1) < lambda ? 1 + (r(2) < lambda * 0.3 ? 1 : 0) : 0
  } else {
    requests = Math.round(lambda * (0.65 + 0.7 * r(1)))
  }
  if (requests <= 0) return null

  const stats = emptyStats(UPSTREAMS.length)
  stats.requests = requests
  if (profile.rejected) {
    stats.client_errors = requests
    stats.cache_unknown = requests
    const p50 = 1.4 + r(3)
    stats.duration_p50_ms = p50
    stats.duration_p90_ms = p50 * 1.8
    stats.duration_p99_ms = p50 * 3
    stats.duration_mean_ms = p50 * 1.2
    return stats
  }

  // An incident window: a few hours a week where GLM throws 5xx.
  const incident = model === "glm-5.3" && noise(Math.floor(t / (6 * HOUR)), 77) < 0.04
  const serverRate = profile.serverErr + (incident ? 0.18 : 0)
  stats.server_errors = Math.min(requests, Math.round(requests * serverRate * (0.5 + r(4)) + (r(5) < serverRate ? 1 : 0)))
  stats.client_errors = Math.min(
    requests - stats.server_errors,
    Math.round(requests * profile.clientErr * (0.5 + r(6))),
  )
  const failed = stats.client_errors + stats.server_errors
  stats.disconnects = Math.min(requests - failed, Math.round(requests * 0.006 * r(7) * 3))
  stats.ok = requests - failed
  stats.streamed = Math.round(stats.ok * (0.82 + 0.15 * r(8)))

  const served = stats.ok
  const prompt = Math.round(served * profile.promptAvg * (0.75 + 0.5 * r(9)))
  const ratio = Math.min(0.985, Math.max(0, profile.cache * (0.86 + 0.24 * r(10)) - (incident ? 0.2 : 0)))
  stats.prompt_tokens = prompt
  stats.cached_tokens = Math.round(prompt * ratio)
  stats.completion_tokens = Math.round(served * profile.completionAvg * (0.7 + 0.6 * r(11)))
  stats.cache_hit_ratio = prompt > 0 ? stats.cached_tokens / prompt : null
  stats.cache_full = Math.round(served * ratio * 0.12)
  stats.cache_cold = Math.round(served * (1 - ratio) * 0.55)
  stats.cache_unknown = Math.round(served * 0.01 * r(12))
  stats.cache_partial = Math.max(0, served - stats.cache_full - stats.cache_cold - stats.cache_unknown)

  if (served > 0) {
    const load = diurnal("office", t)
    const ttft = (110 + (profile.promptAvg / 1000) * 9 * (1 - ratio) * 6 + load * 90) * (0.85 + 0.3 * r(13))
    stats.ttft_p50_ms = ttft
    stats.ttft_p90_ms = ttft * (2.0 + r(14))
    stats.ttft_p99_ms = ttft * (4 + 2 * r(15)) + (incident ? 4000 : 0)
    stats.ttft_mean_ms = ttft * 1.35
    const tps = (MODEL_TPS[model] ?? 60) * (1 - 0.28 * load) * (0.92 + 0.16 * r(16))
    stats.decode_tps_p50 = tps
    stats.decode_tps_p10 = tps * (0.55 + 0.15 * r(17))
    stats.decode_tps_p90 = tps * (1.12 + 0.08 * r(18))
    stats.decode_tps_mean = tps * 0.97
    stats.tpot_p50_ms = 1000 / tps
    stats.tpot_p90_ms = 1000 / stats.decode_tps_p10
    const duration = ttft + (profile.completionAvg / tps) * 1000
    stats.duration_p50_ms = duration
    stats.duration_p90_ms = duration * (2.1 + 0.6 * r(19))
    stats.duration_p99_ms = duration * (4 + 2 * r(20))
    stats.duration_mean_ms = duration * 1.3
  }

  const ups = MODEL_UPSTREAMS[model] ?? [0]
  if (ups.length === 1) {
    stats.upstream_requests[ups[0]] = requests
  } else {
    const share = 0.38 + 0.24 * r(21)
    stats.upstream_requests[ups[0]] = Math.round(requests * share)
    stats.upstream_requests[ups[1]] = requests - stats.upstream_requests[ups[0]]
  }

  const toolShare = key === "a1b2c3d4e5f6" ? 0.34 : 0.12
  const length = Math.round(served * (0.015 + 0.03 * r(22)))
  const tools = Math.round(served * toolShare * (0.8 + 0.4 * r(23)))
  stats.finish_reasons = {
    stop: Math.max(0, served - length - tools),
    tool_calls: tools,
    length,
  }
  return stats
}

function principalKeys(): string[] {
  return Object.keys(profiles)
}

function principalName(key: string): Principal {
  const token = byId.get(key)
  if (token) return { key, name: token.name, kind: "token", status: tokenStatusOf(token, Date.now()) }
  if (key === "master") return { key, name: "master token", kind: "master" }
  if (key === "anonymous") return { key, name: "anonymous", kind: "anonymous" }
  if (key === "rejected") return { key, name: "rejected", kind: "rejected" }
  return { key, name: `rjk_${key}`, kind: "deleted_token" }
}

// ---------------------------------------------------------------- API

const delay = <T,>(value: T, ms = 120): Promise<T> =>
  new Promise((resolve) => setTimeout(() => resolve(value), ms))

function lastUsed(token: ApiToken): number | null {
  const profile = profiles[token.id]
  if (!profile) return token.last_used_ms
  const [from, until] = activeWindow(token.id)
  const now = Date.now()
  const end = Math.min(until, now)
  if (end <= from) return null
  // Busy tokens were used seconds ago; quiet ones some minutes back.
  const gap = profile.shape === "nightly" && diurnal("nightly", end) < 1 ? 5 * HOUR : 20_000
  return Math.max(from, end - gap * noise(hashString(token.id), Math.floor(now / 60_000)))
}

function present(token: ApiToken): ApiToken {
  const now = Date.now()
  const status =
    token.revoked_at_ms != null
      ? "revoked"
      : token.expires_at_ms != null && token.expires_at_ms <= now
        ? "expired"
        : "active"
  return { ...token, status, last_used_ms: lastUsed(token) }
}

export function listTokens(): Promise<TokenList> {
  const sorted = [...tokens].sort((a, b) => b.created_at_ms - a.created_at_ms)
  return delay({ enabled: true, tokens: sorted.map(present) })
}

function validate(name: string | undefined, description: string | undefined, expires: number | null | undefined) {
  if (name !== undefined && (name.trim().length < 1 || name.length > 64)) {
    throw new Error("name must be 1-64 characters")
  }
  if (description !== undefined && description.length > 512) {
    throw new Error("description must be at most 512 characters")
  }
  if (expires != null && expires <= Date.now()) {
    throw new Error("expires_at_ms must be in the future")
  }
}

export async function createToken(body: TokenCreate): Promise<CreatedToken> {
  validate(body.name, body.description, body.expires_at_ms)
  const seed = Math.floor(Math.random() * 2 ** 31)
  const id = hex(seed, 12)
  const token: ApiToken = {
    id,
    prefix: `rjk_${id}`,
    name: body.name.trim(),
    description: body.description ?? "",
    created_at_ms: Date.now(),
    expires_at_ms: body.expires_at_ms,
    revoked_at_ms: null,
    last_used_ms: null,
    status: "active",
  }
  tokens = [token, ...tokens]
  reindex()
  return delay({ token: present(token), secret: `rjk_${id}_${hex(seed + 1, 64)}` }, 250)
}

function find(id: string): ApiToken {
  const token = byId.get(id)
  if (!token) throw new Error("unknown token")
  return token
}

export async function updateToken(id: string, patch: TokenPatch): Promise<ApiToken> {
  validate(patch.name, patch.description, patch.expires_at_ms)
  const token = find(id)
  if (patch.name !== undefined) token.name = patch.name.trim()
  if (patch.description !== undefined) token.description = patch.description
  if (patch.expires_at_ms !== undefined) token.expires_at_ms = patch.expires_at_ms
  return delay(present(token))
}

export async function revokeToken(id: string): Promise<ApiToken> {
  const token = find(id)
  if (token.revoked_at_ms == null) token.revoked_at_ms = Date.now()
  return delay(present(token))
}

export async function deleteToken(id: string): Promise<void> {
  find(id)
  tokens = tokens.filter((candidate) => candidate.id !== id)
  reindex()
  return delay(undefined)
}

export async function getToken(id: string): Promise<ApiToken> {
  return delay(present(find(id)), 60)
}

export function usageMeta(): Promise<UsageMeta> {
  const keys = new Set<string>([...tokens.map((token) => token.id), ...principalKeys()])
  // Existing tokens newest first, then the fixed principals.
  const ordered = [
    ...[...tokens].sort((a, b) => b.created_at_ms - a.created_at_ms).map((t) => t.id),
    ...principalKeys().filter((key) => !byId.has(key)),
  ].filter((key) => keys.has(key))
  return delay({
    principals: ordered.map(principalName),
    models: MODELS,
    upstreams: UPSTREAMS,
    minute_retention_hours: 24,
    hour_retention_days: 90,
    recent_capacity: 100_000,
    tokens_enabled: true,
  })
}

export function usageSeries(params: SeriesParams): Promise<SeriesResponse> {
  const { granularity, group_by: groupBy, tz_offset_min: tz } = params
  const now = Date.now()
  const retention = granularity === "minute" ? 24 * HOUR : 90 * DAY
  const fromMs = Math.max(params.from_ms, now - retention)
  const toMs = Math.min(params.to_ms, now)
  const bucketMs = BUCKET_MS[granularity]
  // Day buckets are folded from hourly ones, exactly as the backend does.
  const stepMs = granularity === "day" ? HOUR : bucketMs
  const principals = principalKeys().filter((key) => !params.token || key === params.token)
  const models = MODELS.filter((model) => !params.model || model === params.model)

  const groups = new Map<string, Map<number, Acc>>()
  const groupTotals = new Map<string, Acc>()
  const overall = newAcc()
  const first = bucketFloor(fromMs, granularity, tz)
  for (let t = Math.floor(first / stepMs) * stepMs; t < toMs; t += stepMs) {
    const bucket = bucketFloor(t, granularity, tz)
    for (const key of principals) {
      for (const model of models) {
        const stats = bucketStats(key, model, t, stepMs)
        if (!stats) continue
        const group = groupBy === "token" ? key : groupBy === "model" ? model : "all"
        let points = groups.get(group)
        if (!points) groups.set(group, (points = new Map()))
        let acc = points.get(bucket)
        if (!acc) points.set(bucket, (acc = newAcc()))
        addTo(acc, stats)
        let total = groupTotals.get(group)
        if (!total) groupTotals.set(group, (total = newAcc()))
        addTo(total, stats)
        addTo(overall, stats)
      }
    }
  }

  // Exactly the backend's fold: with group_by=token and no token filter,
  // the `limit` busiest principals stay individual and everything else
  // becomes one `__other` group (other = total - returned).
  const groupCount = groups.size
  if (groupBy === "token" && !params.token) {
    const limit = Math.min(Math.max(params.limit ?? 20, 1), 500)
    const ranked = [...groupTotals.entries()].sort((a, b) => b[1].stats.requests - a[1].stats.requests)
    const folded = ranked.slice(limit).map(([key]) => key)
    if (folded.length > 0) {
      const otherPoints = new Map<number, Acc>()
      const otherTotal = newAcc()
      for (const key of folded) {
        for (const [t, acc] of groups.get(key) ?? []) {
          let into = otherPoints.get(t)
          if (!into) otherPoints.set(t, (into = newAcc()))
          mergeAcc(into, acc)
        }
        mergeAcc(otherTotal, groupTotals.get(key)!)
        groups.delete(key)
        groupTotals.delete(key)
      }
      groups.set("__other", otherPoints)
      groupTotals.set("__other", otherTotal)
    }
  }

  const series = [...groups.entries()].map(([key, points]) => ({
    key,
    points: [...points.entries()]
      .sort(([a], [b]) => a - b)
      .map(([t, acc]): StatsPoint => ({ t, ...finish(acc) })),
  }))
  const totals = [...groupTotals.entries()]
    .map(([key, acc]) => ({ key, ...finish(acc) }))
    .sort((a, b) =>
      a.key === "__other" ? 1 : b.key === "__other" ? -1 : b.requests - a.requests,
    )
  return delay(
    {
      granularity,
      bucket_ms: bucketMs,
      from_ms: fromMs,
      to_ms: toMs,
      group_by: groupBy,
      group_count: groupCount,
      series,
      totals,
      overall: finish(overall),
    },
    180,
  )
}

const ENDPOINTS = ["chat", "chat", "chat", "messages", "completions"]
const ROUTES = ["affinity", "affinity", "affinity", "least_loaded", "exact_prefix", "spill"]

export function recentRequests(params: RecentParams): Promise<RecentRequests> {
  const limit = Math.min(Math.max(params.limit, 1), 1000)
  const out: RequestRecord[] = []
  const before = params.before_ms ?? Date.now() + 1
  const end = Math.floor((before - 1) / 1000)
  const keys = principalKeys()
  // Walk back second by second; each second holds zero to three requests
  // whose identity is a pure function of the timestamp.
  // A filtered view scans further back so a quiet token still fills a page.
  const span = params.token || params.errors_only ? 24 * 3600 : 6 * 3600
  for (let s = end; s > end - span && out.length < limit; s--) {
    const t = s * 1000
    const busy = 0.25 + 0.75 * diurnal("office", t)
    const count = noise(s, 1) < 0.45 * busy ? 1 + Math.floor(noise(s, 2) * 2.4) : 0
    const second: RequestRecord[] = []
    for (let i = 0; i < count; i++) {
      const record = synthRequest(t + Math.floor(noise(s, i, 3) * 999), keys, s, i)
      if (record) second.push(record)
    }
    // Newest first inside the second too, so `before_ms` paging is exact.
    second.sort((a, b) => b.t_ms - a.t_ms)
    for (const record of second) {
      if (out.length >= limit) break
      // `before_ms` is exclusive: page from strictly older rows.
      if (record.t_ms >= before) continue
      if (params.token && record.principal !== params.token) continue
      if (params.model && record.model !== params.model) continue
      if (params.errors_only && record.status < 400 && record.result === "complete") continue
      out.push(record)
    }
  }
  return delay({ capacity: 100_000, requests: out }, 90)
}

function synthRequest(t: number, keys: string[], s: number, i: number): RequestRecord | null {
  const r = (salt: number) => noise(s, i, salt)
  // Pick a principal weighted by its current rate.
  const shapes = {
    office: diurnal("office", t),
    nightly: diurnal("nightly", t),
    flat: diurnal("flat", t),
  }
  const weights = keys.map((key) => {
    const [from, until] = activeWindow(key)
    if (t < from || t >= until) return 0
    const profile = profiles[key]
    return (profile.rate[MODELS[0]] + (profile.rate[MODELS[1]] ?? 0)) * shapes[profile.shape]
  })
  const sum = weights.reduce((a, b) => a + b, 0)
  if (sum <= 0) return null
  let pick = r(4) * sum
  let index = 0
  while (index < keys.length - 1 && pick >= weights[index]) {
    pick -= weights[index]
    index += 1
  }
  const key = keys[index]
  const profile = profiles[key]
  const modelWeights = MODELS.map((model) => profile.rate[model] ?? 0)
  const model = r(5) * (modelWeights[0] + modelWeights[1]) < modelWeights[0] ? MODELS[0] : MODELS[1]
  const endpoint = ENDPOINTS[Math.floor(r(6) * ENDPOINTS.length)]
  const stream = r(7) < 0.85
  const base: RequestRecord = {
    t_ms: t,
    principal: key,
    model,
    endpoint,
    upstream: null,
    status: 401,
    stream,
    result: "auth_rejected",
    prompt_tokens: null,
    cached_tokens: null,
    completion_tokens: null,
    cache_outcome: "unknown",
    ttft_ms: null,
    duration_ms: 0.8 + r(8) * 2,
    decode_tps: null,
    finish_reason: null,
    route_outcome: null,
    request_bytes: Math.round(2000 + r(9) * 40_000),
    response_bytes: 52,
  }
  if (profile.rejected) return base

  const ups = MODEL_UPSTREAMS[model]
  const upstream = ups[Math.floor(r(10) * ups.length)]
  const roll = r(11)
  const prompt = Math.round(profile.promptAvg * (0.3 + 1.4 * r(12)))
  const ratio = Math.min(0.99, Math.max(0, profile.cache + (r(13) - 0.5) * 0.4))
  const cached = r(14) < 0.12 ? 0 : Math.floor((prompt * ratio) / 256) * 256
  const outcome = cached === 0 ? "cold" : cached >= prompt - 256 ? "full" : "partial"
  const tps = (MODEL_TPS[model] ?? 60) * (0.6 + 0.6 * r(15))
  const ttft = 90 + ((prompt - cached) / 1000) * 55 * (0.8 + 0.4 * r(16))
  const completion = Math.round(profile.completionAvg * (0.2 + 1.6 * r(17)))
  const duration = ttft + (completion / tps) * 1000
  const finishRoll = r(18)
  const record: RequestRecord = {
    ...base,
    upstream,
    status: 200,
    result: "complete",
    prompt_tokens: prompt,
    cached_tokens: cached,
    completion_tokens: completion,
    cache_outcome: outcome,
    ttft_ms: stream ? ttft : null,
    duration_ms: duration,
    decode_tps: completion > 1 ? tps : null,
    finish_reason: finishRoll < 0.03 ? "length" : finishRoll < 0.3 ? "tool_calls" : "stop",
    route_outcome: ROUTES[Math.floor(r(19) * ROUTES.length)],
    request_bytes: Math.round(prompt * 4.1),
    response_bytes: Math.round(completion * 5.5 + 300),
  }
  if (roll < profile.serverErr * 4) {
    return {
      ...record,
      status: r(20) < 0.5 ? 502 : 503,
      result: "upstream_error",
      completion_tokens: null,
      cached_tokens: null,
      prompt_tokens: null,
      cache_outcome: "unknown",
      ttft_ms: null,
      decode_tps: null,
      finish_reason: null,
      duration_ms: 20 + r(21) * 400,
      response_bytes: 120,
    }
  }
  if (roll < profile.serverErr * 4 + profile.clientErr * 4) {
    return {
      ...record,
      status: r(22) < 0.6 ? 400 : 413,
      result: "complete",
      completion_tokens: null,
      cached_tokens: null,
      prompt_tokens: null,
      cache_outcome: "unknown",
      ttft_ms: null,
      decode_tps: null,
      finish_reason: null,
      duration_ms: 4 + r(23) * 30,
      response_bytes: 180,
    }
  }
  if (roll < profile.serverErr * 4 + profile.clientErr * 4 + 0.01) {
    return { ...record, result: "client_disconnect", finish_reason: null, completion_tokens: Math.round(completion / 3) }
  }
  if (roll > 0.995) {
    return { ...record, result: "upstream_read_error", status: 200, finish_reason: null }
  }
  return record
}
