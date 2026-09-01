import type { GpuSample, Sample } from "@/lib/api"

/**
 * A GPU utilization reading is a short NVML observation taken only once per
 * machine-view scrape. Token rates, meanwhile, cover the whole scrape
 * interval. Average a few recent GPU observations before putting the two next
 * to each other so a sample that lands between kernels does not look like an
 * idle engine.
 */
export const GPU_UTIL_WINDOW_MS = 15_000

/**
 * Returns the newest GPU inventory with utilization replaced by a bounded
 * trailing mean. Missing observations are skipped rather than interpreted as
 * zero, and an inventory older than the window is not carried forward.
 */
export function smoothedGpus(
  points: Sample[],
  latest: Sample | null,
  windowMs = GPU_UTIL_WINDOW_MS,
): GpuSample[] {
  const anchor = latest?.t ?? points.at(-1)?.t
  if (anchor == null) return []

  const byTimestamp = new Map<number, Sample>()
  for (const sample of points) byTimestamp.set(sample.t, sample)
  if (latest != null) byTimestamp.set(latest.t, latest)

  const from = anchor - windowMs
  const recent = [...byTimestamp.values()]
    .filter((sample) => sample.t >= from && sample.t <= anchor)
    .sort((left, right) => left.t - right.t)
  let inventory: GpuSample[] | undefined
  for (let index = recent.length - 1; index >= 0; index -= 1) {
    if ((recent[index].gpus?.length ?? 0) > 0) {
      inventory = recent[index].gpus
      break
    }
  }
  if (inventory == null) return []

  return inventory.map((gpu) => {
    const values = recent.flatMap((sample) => {
      const value = sample.gpus?.find((candidate) => candidate.index === gpu.index)?.util_pct
      return typeof value === "number" && Number.isFinite(value) ? [value] : []
    })
    return {
      ...gpu,
      util_pct:
        values.length > 0
          ? values.reduce((sum, value) => sum + value, 0) / values.length
          : null,
    }
  })
}
