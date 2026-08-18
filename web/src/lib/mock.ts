// Synthetic data for UI development without a live load balancer:
// `npm run dev` then open with ?mock=1 (or VITE_MOCK=1 npm run dev).
//
// The generator emulates the node06 shape: 8 GPUs in two TP4 pairs behind
// two vLLM engines, bursty agent traffic with quiet windows, and energy
// that integrates from power draw.

import type { Sample, Series, Summary, TokenBucket, TokenHistory } from "./api"

const START = Date.now() - 24 * 3600 * 1000
const STEP_MS = 30_000
const UPSTREAMS = ["http://127.0.0.1:8012", "http://127.0.0.1:8013"]
const GPU_NAME = "NVIDIA RTX PRO 6000 Blackwell"

// Deterministic pseudo-random so the mock is stable across reloads.
function prng(seed: number): () => number {
  let state = seed >>> 0
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0
    return state / 0xffffffff
  }
}

function buildSamples(): Sample[] {
  const random = prng(20260815)
  const samples: Sample[] = []
  let wattHours = 0
  const count = Math.floor((24 * 3600 * 1000) / STEP_MS)
  for (let i = 0; i < count; i++) {
    const t = START + i * STEP_MS
    const hour = (t / 3600000) % 24
    // Load shape: busy working hours, a deep quiet window, random bursts.
    const daily = Math.max(0, Math.sin(((hour - 6) / 24) * Math.PI * 2) * 0.6 + 0.4)
    const burst = random() < 0.06 ? random() * 0.7 : 0
    const load = Math.min(1, daily * 0.7 + burst + random() * 0.08)
    const inflight = Math.round(load * 22 + random() * 2)
    const runningA = Math.round(inflight * 0.55)
    const runningB = inflight - runningA
    const genTps = load * 1600 + random() * 120
    const promptTps = load * 5200 + burst * 9000 + random() * 300
    const cachedTps = promptTps * (0.55 + random() * 0.25)
    const gpuBase = load * 78 + random() * 8
    const gpus = Array.from({ length: 8 }, (_, index) => {
      const hot = index === 1 ? 6 : 0 // GPU1 runs hotter than its neighbours
      const util = Math.max(0, Math.min(100, gpuBase + random() * 14 - 7))
      const power = 90 + (util / 100) * 480 + random() * 25
      return {
        index,
        name: GPU_NAME,
        util_pct: util,
        mem_used_bytes: (62 + load * 24 + random() * 3) * 2 ** 30,
        mem_total_bytes: 96 * 2 ** 30,
        power_watts: power,
        temp_c: 38 + (util / 100) * 30 + hot + random() * 3,
        sm_mhz: util > 5 ? 2400 + random() * 217 : 345,
        mem_util_pct: util * (0.5 + random() * 0.2),
        mem_clock_mhz: util > 5 ? 10251 : 810,
        power_limit_watts: 600,
        fan_pct: 30 + (util / 100) * 45,
        pstate: util > 5 ? 0 : 8,
        temp_mem_c: 44 + (util / 100) * 34 + hot,
        throttle_sw_power: power > 560 ? 1 : 0,
        throttle_sw_thermal: 0,
        throttle_hw_thermal: 0,
        throttle_hw: 0,
      }
    })
    const gpuWatts = gpus.reduce((sum, gpu) => sum + (gpu.power_watts ?? 0), 0)
    const cpuWatts = 120 + load * 180 + random() * 20
    wattHours += ((gpuWatts + cpuWatts) * STEP_MS) / 3600000
    samples.push({
      t,
      host: {
        cpu_pct: Math.min(100, 6 + load * 38 + burst * 20 + random() * 6),
        load1: 2 + load * 30 + random() * 2,
        mem_total_bytes: 1024 * 2 ** 30,
        mem_used_bytes: (700 + load * 90 + random() * 10) * 2 ** 30,
        mem_cached_bytes: (140 + random() * 30) * 2 ** 30,
        swap_total_bytes: 64 * 2 ** 30,
        swap_used_bytes: 2.1 * 2 ** 30,
        net_rx_bps: load * 90e6 + random() * 8e6,
        net_tx_bps: load * 220e6 + random() * 15e6,
        disk_read_bps: burst * 900e6 + random() * 30e6,
        disk_write_bps: load * 60e6 + random() * 20e6,
        disk_read_iops: burst * 40_000 + random() * 800,
        disk_write_iops: load * 4_000 + burst * 12_000 + random() * 400,
        disk_util_pct: Math.min(100, load * 35 + burst * 55 + random() * 8),
        disk_inflight: Math.round(load * 4 + burst * 18 + random() * 2),
        iowait_pct: Math.min(40, load * 4 + burst * 18 + random() * 2),
        io_pressure_pct: Math.min(80, load * 6 + burst * 28 + random() * 3),
        mem_pressure_pct: Math.min(40, load * 2 + burst * 8 + random()),
        dirty_bytes: (0.4 + load * 1.2 + burst * 3) * 2 ** 30,
        writeback_bytes: (0.05 + burst * 0.4) * 2 ** 30,
        cpu_watts: cpuWatts,
        disks: [
          {
            mount: "/",
            total_bytes: 1.8e12,
            used_bytes: 1.02e12,
            inodes_total: 110e6,
            inodes_used: 12e6,
          },
          {
            mount: "/models",
            total_bytes: 7.6e12,
            used_bytes: 5.9e12,
            inodes_total: 480e6,
            inodes_used: 18e6,
          },
        ],
      },
      gpus,
      serving: {
        inflight,
        requests_per_second: load * 9 + random(),
        prompt_tps: promptTps,
        gen_tps: genTps,
        cached_tps: cachedTps,
        ttft_p50_ms: 140 + load * 420 + random() * 60,
        ttft_p95_ms: 320 + load * 2400 + burst * 1800 + random() * 250,
        tpot_p95_ms: 18 + load * 14 + random() * 4,
        cache_hit_pct: 52 + random() * 25,
        cache_hit_source: "response_usage" as const,
        upstreams: UPSTREAMS.map((name, index) => ({
          name,
          up: 1,
          inflight: index === 0 ? runningA : runningB,
          requests_per_second: (load * 9) / 2 + random() * 0.6,
        })),
      },
      engines: UPSTREAMS.map((endpoint, index) => ({
        endpoint,
        running: index === 0 ? runningA : runningB,
        waiting: burst > 0.3 ? Math.round(random() * 6) : 0,
        kv_cache_pct: Math.min(97, 18 + load * 58 + random() * 9 + index * 4),
        gen_tps: genTps / 2 + random() * 60,
        prompt_tps: promptTps / 2 + random() * 150,
        prefix_hit_pct: 45 + random() * 35,
      })),
      energy: {
        gpu_watts: gpuWatts,
        cpu_watts: cpuWatts,
        total_watt_hours: wattHours,
      },
    })
  }
  return samples
}

let cache: Sample[] | null = null

function samples(): Sample[] {
  cache ??= buildSamples()
  return cache
}

export function mockSummary(): Summary {
  const all = samples()
  return {
    now: Date.now(),
    hostname: "node06",
    interval_ms: 5000,
    retention_seconds: 86400,
    upstreams: UPSTREAMS,
    latest: all[all.length - 1] ?? null,
  }
}

const HOUR_MS = 3_600_000

/**
 * A month of hourly token volume with the shape the heatmaps exist to show:
 * office hours on weekdays, a quiet weekend, one dead day (an outage), and a
 * short window before the history starts so the "no data" cell is exercised.
 */
function buildTokenBuckets(days: number): TokenBucket[] {
  const random = prng(20260817)
  const now = Date.now()
  const first = Math.floor((now - days * 24 * HOUR_MS) / HOUR_MS) * HOUR_MS
  const buckets: TokenBucket[] = []
  for (let t = first; t <= now; t += HOUR_MS) {
    const date = new Date(t)
    const hour = date.getHours()
    const weekday = date.getDay()
    const dayIndex = Math.floor((t - first) / (24 * HOUR_MS))
    if (dayIndex === 3) continue // an outage day: no buckets at all
    const weekend = weekday === 0 || weekday === 6
    const office = Math.max(0, Math.sin(((hour - 7) / 15) * Math.PI))
    const intensity = office * (weekend ? 0.25 : 1) * (0.6 + random() * 0.8)
    if (intensity < 0.05) {
      buckets.push({ t, prompt: 0, completion: 0, cached: 0, requests: 0 })
      continue
    }
    const requests = Math.round(intensity * 40 + random() * 6)
    const prompt = requests * (14_000 + random() * 40_000)
    buckets.push({
      t,
      prompt,
      completion: requests * (300 + random() * 700),
      cached: prompt * (0.45 + random() * 0.45),
      requests,
    })
  }
  return buckets
}

export function mockTokens(days: number): TokenHistory {
  return {
    now: Date.now(),
    days,
    bucket_seconds: 3600,
    buckets: buildTokenBuckets(days),
  }
}

export function mockSeries(rangeSeconds: number, points: number): Series {
  const now = Date.now()
  const start = now - rangeSeconds * 1000
  const selected = samples().filter((sample) => sample.t >= start)
  const stride = Math.max(1, Math.ceil(selected.length / points))
  return {
    now,
    range_seconds: rangeSeconds,
    points: selected.filter((_, index) => index % stride === 0),
  }
}
