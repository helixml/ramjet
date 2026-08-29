// Types mirror the Rust `machineview` serde model exactly (snake_case JSON).

export interface DiskSample {
  mount: string
  total_bytes: number
  used_bytes: number
  inodes_total?: number | null
  inodes_used?: number | null
}

export interface HostSample {
  cpu_pct: number | null
  load1: number | null
  mem_total_bytes: number | null
  mem_used_bytes: number | null
  mem_cached_bytes: number | null
  swap_total_bytes: number | null
  swap_used_bytes: number | null
  dirty_bytes?: number | null
  writeback_bytes?: number | null
  net_rx_bps: number | null
  net_tx_bps: number | null
  disk_read_bps: number | null
  disk_write_bps: number | null
  disk_read_iops?: number | null
  disk_write_iops?: number | null
  disk_util_pct?: number | null
  disk_inflight?: number | null
  iowait_pct?: number | null
  io_pressure_pct?: number | null
  mem_pressure_pct?: number | null
  cpu_watts: number | null
  disks?: DiskSample[]
}

export interface GpuSample {
  index: number
  name: string
  util_pct: number | null
  mem_used_bytes: number | null
  mem_total_bytes: number | null
  power_watts: number | null
  temp_c: number | null
  sm_mhz: number | null
  mem_util_pct?: number | null
  mem_clock_mhz?: number | null
  power_limit_watts?: number | null
  fan_pct?: number | null
  pstate?: number | null
  temp_mem_c?: number | null
  throttle_sw_power?: number | null
  throttle_sw_thermal?: number | null
  throttle_hw_thermal?: number | null
  throttle_hw?: number | null
}

export interface UpstreamSample {
  name: string
  up: number | null
  inflight: number | null
  requests_per_second: number | null
}

export interface ServingSample {
  inflight: number | null
  requests_per_second: number | null
  prompt_tps: number | null
  gen_tps: number | null
  cached_tps: number | null
  ttft_p50_ms: number | null
  ttft_p95_ms: number | null
  tpot_p95_ms: number | null
  /** Median per-stream decode rate over the histogram window. */
  stream_tps_p50?: number | null
  /** Slowest-5% per-stream decode rate — the tail a user feels. */
  stream_tps_p05?: number | null
  cache_hit_pct: number | null
  /** Which layer `cache_hit_pct`/`cached_tps` came from. Absent when both are. */
  cache_hit_source?: CacheHitSource | null
  upstreams?: UpstreamSample[]
}

/**
 * `response_usage` is the proxy's own token-weighted figure, measured on the
 * served responses. `engine_prefix_cache` is the fallback for engines that
 * never populate `prompt_tokens_details.cached_tokens`: it counts every query
 * the engines saw, including traffic this proxy did not route.
 */
export type CacheHitSource = "response_usage" | "engine_prefix_cache"

export interface EngineSample {
  endpoint: string
  running: number | null
  waiting: number | null
  kv_cache_pct: number | null
  gen_tps: number | null
  prompt_tps: number | null
  prefix_hit_pct: number | null
}

export interface EnergySample {
  gpu_watts: number | null
  cpu_watts: number | null
  total_watt_hours: number
}

export interface Sample {
  t: number
  host?: HostSample
  gpus?: GpuSample[]
  serving?: ServingSample
  engines?: EngineSample[]
  energy?: EnergySample
}

export interface Summary {
  now: number
  hostname: string | null
  interval_ms: number
  retention_seconds: number
  upstreams: string[]
  latest: Sample | null
}

export interface Series {
  now: number
  range_seconds: number
  points: Sample[]
}

/** One UTC hour of token volume. Grouping into local days happens here. */
export interface TokenBucket {
  t: number
  prompt: number
  completion: number
  cached: number
  requests: number
}

export interface TokenHistory {
  now: number
  days: number
  bucket_seconds: number
  buckets: TokenBucket[]
}

export type AdaptiveMode = "off" | "manual" | "recommend" | "auto"
export type AdaptivePhase =
  | "idle"
  | "draining"
  | "stopping"
  | "starting"
  | "stabilizing"
  | "rolling_back"
  | "failed"

export interface AdaptiveEngine {
  upstream: number
  label: string
  container: string
  image: string
  gpus: number[]
}

export interface AdaptiveProfile {
  id: string
  label: string
  description: string
  engines: AdaptiveEngine[]
  active: boolean
}

export interface AdaptiveTransition {
  from: string
  to: string
  automatic: boolean
  allow_downtime: boolean
  requires_downtime: boolean
  estimated_downtime_seconds: number
  condition?: {
    metric:
      | "requests_per_second"
      | "prompt_tokens_per_second"
      | "completion_tokens_per_second"
      | "tokens_per_second"
      | "inflight"
      | "load_per_engine"
    comparison: "above" | "below"
    threshold: number
    for_seconds: number
  } | null
}

export interface AdaptiveStatus {
  enabled: boolean
  mode: AdaptiveMode
  active_profile: string
  phase: AdaptivePhase
  target_profile: string | null
  phase_started_at: number
  last_error: string | null
  signal: {
    requests_per_second: number
    prompt_tokens_per_second: number
    completion_tokens_per_second: number
    tokens_per_second: number
    inflight: number
    load_per_engine: number
  }
  recommendation: string | null
  profiles: AdaptiveProfile[]
  transitions: AdaptiveTransition[]
}

/** Frames pushed over `/api/machineview/stream`, tagged by `kind`. */
export type StreamFrame =
  | {
      kind: "hello"
      now: number
      hostname: string | null
      interval_ms: number
      stream_interval_ms: number
      retention_seconds: number
      upstreams: string[]
    }
  | { kind: "serving"; t: number; serving: ServingSample }
  | { kind: "sample"; sample: Sample }

/** Absolute stream URL for the page's own origin, http(s) → ws(s). */
export function streamUrl(): string {
  const url = new URL("/api/machineview/stream", window.location.href)
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:"
  return url.toString()
}

export function isMockMode(): boolean {
  if (import.meta.env.VITE_MOCK === "1") return true
  return new URLSearchParams(window.location.search).has("mock")
}

async function getJson<T>(path: string): Promise<T> {
  const response = await fetch(path, { headers: { accept: "application/json" } })
  if (!response.ok) {
    throw new Error(`${path}: HTTP ${response.status}`)
  }
  return (await response.json()) as T
}

export function fetchSummary(): Promise<Summary> {
  return getJson<Summary>("/api/machineview/summary")
}

export function fetchSeries(rangeSeconds: number, points: number): Promise<Series> {
  return getJson<Series>(
    `/api/machineview/series?range=${rangeSeconds}&points=${points}`,
  )
}

export function fetchTokens(days: number): Promise<TokenHistory> {
  return getJson<TokenHistory>(`/api/machineview/tokens?days=${days}`)
}

export async function fetchAdaptiveStatus(): Promise<AdaptiveStatus | null> {
  if (isMockMode()) {
    return {
      enabled: true,
      mode: "recommend",
      active_profile: "split-tp4",
      phase: "idle",
      target_profile: null,
      phase_started_at: Date.now() / 1000,
      last_error: null,
      signal: {
        requests_per_second: 3.7,
        prompt_tokens_per_second: 1820,
        completion_tokens_per_second: 635,
        tokens_per_second: 2455,
        inflight: 5,
        load_per_engine: 2.5,
      },
      recommendation: "unified-tp8",
      profiles: [
        {
          id: "split-tp4",
          label: "Twin Cruise",
          description: "Two TP4 engines for sustained throughput and cache locality.",
          active: true,
          engines: [
            { upstream: 0, label: "A", container: "engine-a", image: "sha256:mock", gpus: [0, 1, 2, 3] },
            { upstream: 1, label: "B", container: "engine-b", image: "sha256:mock", gpus: [4, 5, 6, 7] },
          ],
        },
        {
          id: "unified-tp8",
          label: "Afterburner",
          description: "One TP8 engine spanning the box for burst latency.",
          active: false,
          engines: [
            { upstream: 2, label: "Aero", container: "engine-tp8", image: "sha256:mock", gpus: [0, 1, 2, 3, 4, 5, 6, 7] },
          ],
        },
      ],
      transitions: [
        {
          from: "split-tp4",
          to: "unified-tp8",
          automatic: true,
          allow_downtime: true,
          requires_downtime: true,
          estimated_downtime_seconds: 540,
          condition: { metric: "completion_tokens_per_second", comparison: "above", threshold: 400, for_seconds: 30 },
        },
      ],
    }
  }
  const response = await fetch("/api/adaptive/status", {
    headers: { accept: "application/json" },
  })
  if (response.status === 404) return null
  if (!response.ok) throw new Error(`adaptive status: HTTP ${response.status}`)
  return (await response.json()) as AdaptiveStatus
}

async function postAdaptive<T>(path: string, token: string, body: unknown): Promise<T> {
  const response = await fetch(path, {
    method: "POST",
    headers: {
      accept: "application/json",
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(body),
  })
  if (!response.ok) {
    const payload = (await response.json().catch(() => null)) as { error?: string } | null
    throw new Error(payload?.error ?? `HTTP ${response.status}`)
  }
  return (await response.json()) as T
}

export function setAdaptiveMode(
  mode: AdaptiveMode,
  token: string,
): Promise<AdaptiveStatus> {
  return postAdaptive("/api/adaptive/mode", token, { mode })
}

export function startAdaptiveTransition(
  profile: string,
  token: string,
): Promise<AdaptiveStatus> {
  return postAdaptive("/api/adaptive/transition", token, { profile })
}
