export interface SparkPoint {
  t: number
  v: number | null
}

export const TILE_WINDOW_MS = 30_000
const SPARKLINE_BUCKETS = 24

type Timed = { t: number } & Record<string, number | null>

/** Mean of a series in the trailing window. Nulls are skipped, zeros are not. */
export function windowMean(rows: Timed[], key: string, now: number, windowMs = TILE_WINDOW_MS): number | null {
  const from = now - windowMs
  let sum = 0
  let count = 0
  for (let index = rows.length - 1; index >= 0; index--) {
    const row = rows[index]
    if (row.t < from) break
    const value = row[key]
    if (typeof value !== "number" || !Number.isFinite(value)) continue
    sum += value
    count++
  }
  return count === 0 ? null : sum / count
}

/** Peak of a series in the trailing window. Nulls are skipped, zeros are not. */
export function windowMax(rows: Timed[], key: string, now: number, windowMs = TILE_WINDOW_MS): number | null {
  const from = now - windowMs
  let max: number | null = null
  for (let index = rows.length - 1; index >= 0; index--) {
    const row = rows[index]
    if (row.t < from) break
    const value = row[key]
    if (typeof value !== "number" || !Number.isFinite(value)) continue
    max = max == null ? value : Math.max(max, value)
  }
  return max
}

/**
 * Even time buckets over `windowMs` ending at `now`. Each bucket keeps the
 * max sample in that slice so a 1 Hz burst is not lost between zeros, and
 * so the shape does not jump when a new point arrives.
 */
export function sparkline(
  rows: Timed[],
  key: string,
  now: number,
  windowMs: number,
  buckets = SPARKLINE_BUCKETS,
): SparkPoint[] {
  if (rows.length === 0 || windowMs <= 0 || buckets <= 0) return []
  const from = now - windowMs
  const width = windowMs / buckets
  const points: SparkPoint[] = Array.from({ length: buckets }, (_, index) => ({
    t: from + (index + 1) * width,
    v: null,
  }))
  for (const row of rows) {
    if (row.t < from || row.t > now) continue
    const index = Math.min(buckets - 1, Math.max(0, Math.floor((row.t - from) / width)))
    const value = row[key]
    if (typeof value !== "number" || !Number.isFinite(value)) continue
    const current = points[index]
    if (current.v == null || value >= current.v) {
      current.v = value
      current.t = row.t
    }
  }
  return points
}
