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
  cache_hit_pct: number | null
  upstreams?: UpstreamSample[]
}

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
