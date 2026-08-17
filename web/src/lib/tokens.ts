// Shapes the hourly token history into the two magnitude grids.
//
// The load balancer stores UTC hour buckets so the series is unambiguous;
// everything here is deliberately in the *viewer's* local time, because the
// question these charts answer ("when does this box get used?") is a
// wall-clock question.

import type { TokenBucket } from "@/lib/api"
import type { HeatmapCell } from "@/components/Heatmap"
import { fmtCount } from "@/lib/format"

const DAY_MS = 86_400_000
/** Monday-first, matching ISO weeks and the way a work week reads. */
export const WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]

export interface TokenTotals {
  prompt: number
  completion: number
  cached: number
  requests: number
  total: number
}

export function totals(buckets: TokenBucket[]): TokenTotals {
  return buckets.reduce<TokenTotals>(
    (sum, bucket) => ({
      prompt: sum.prompt + bucket.prompt,
      completion: sum.completion + bucket.completion,
      cached: sum.cached + bucket.cached,
      requests: sum.requests + bucket.requests,
      total: sum.total + bucket.prompt + bucket.completion,
    }),
    { prompt: 0, completion: 0, cached: 0, requests: 0, total: 0 },
  )
}

function startOfLocalDay(t: number): number {
  const date = new Date(t)
  date.setHours(0, 0, 0, 0)
  return date.getTime()
}

/** Monday-index of a local date: Mon = 0 … Sun = 6. */
function weekdayIndex(date: Date): number {
  return (date.getDay() + 6) % 7
}

/** Whole days between two local midnights, DST-safe (they can differ by ±1h). */
function daysBetween(from: number, to: number): number {
  return Math.round((to - from) / DAY_MS)
}

function metricsFor(bucket: TokenTotals): Array<{ label: string; value: string }> {
  return [
    { label: "prompt", value: fmtCount(bucket.prompt) },
    { label: "cached", value: fmtCount(bucket.cached) },
    { label: "generated", value: fmtCount(bucket.completion) },
    { label: "requests", value: fmtCount(bucket.requests) },
  ]
}

export interface Grid {
  rowLabels: string[]
  columnLabels: Array<string | null>
  cells: HeatmapCell[]
}

/** Hours per row of the day grid: 24 rows would not fit, 1 hides the shape. */
const BAND_HOURS = 3
const BANDS = Array.from({ length: 24 / BAND_HOURS }, (_, band) => {
  const start = band * BAND_HOURS
  return `${String(start).padStart(2, "0")}–${String(start + BAND_HOURS).padStart(2, "0")}`
})

/**
 * Calendar grid: one column per date, one row per three-hour band.
 *
 * A day is one number, which as a single row of 30 cells wastes the width
 * and hides everything about *when* in the day the work happened. Splitting
 * each column into bands keeps the same daily total readable down a column
 * while showing the shift pattern across it.
 *
 * A band with no recorded hour keeps a `null` value — an LB that was down is
 * not three hours of zero traffic, and the two must not look the same.
 */
export function dayGrid(buckets: TokenBucket[], now: number): Grid {
  if (buckets.length === 0) {
    return { rowLabels: BANDS, columnLabels: [], cells: [] }
  }
  const byCell = new Map<string, TokenTotals>()
  for (const bucket of buckets) {
    const date = new Date(bucket.t)
    const key = `${startOfLocalDay(bucket.t)}:${Math.floor(date.getHours() / BAND_HOURS)}`
    const current = byCell.get(key) ?? totals([])
    byCell.set(key, {
      prompt: current.prompt + bucket.prompt,
      completion: current.completion + bucket.completion,
      cached: current.cached + bucket.cached,
      requests: current.requests + bucket.requests,
      total: current.total + bucket.prompt + bucket.completion,
    })
  }

  const firstDay = startOfLocalDay(Math.min(...buckets.map((bucket) => bucket.t)))
  const lastDay = startOfLocalDay(now)
  const columns = daysBetween(firstDay, lastDay) + 1

  const cells: HeatmapCell[] = []
  const columnLabels: Array<string | null> = []
  for (let column = 0; column < columns; column++) {
    const date = new Date(firstDay)
    date.setDate(date.getDate() + column)
    const day = date.getTime()
    // Label every other column; at 30 columns every one collides.
    columnLabels.push(
      column % 2 === 0
        ? date.toLocaleDateString("en-GB", { day: "numeric", month: "short" })
        : null,
    )
    for (let row = 0; row < BANDS.length; row++) {
      const bandTotals = byCell.get(`${day}:${row}`)
      cells.push({
        row,
        column,
        label: `${date.toLocaleDateString("en-GB", {
          weekday: "short",
          day: "numeric",
          month: "short",
        })} ${BANDS[row]}`,
        value: bandTotals ? bandTotals.total : null,
        metrics: bandTotals ? metricsFor(bandTotals) : undefined,
      })
    }
  }

  return { rowLabels: BANDS, columnLabels, cells }
}

/**
 * Punchcard grid: one row per weekday, one column per hour of the day,
 * summed over the whole window.
 */
export function hourGrid(buckets: TokenBucket[]): Grid {
  const sums = new Map<string, TokenTotals>()
  for (const bucket of buckets) {
    const date = new Date(bucket.t)
    const key = `${weekdayIndex(date)}:${date.getHours()}`
    const current = sums.get(key)
    sums.set(key, {
      prompt: (current?.prompt ?? 0) + bucket.prompt,
      completion: (current?.completion ?? 0) + bucket.completion,
      cached: (current?.cached ?? 0) + bucket.cached,
      requests: (current?.requests ?? 0) + bucket.requests,
      total: (current?.total ?? 0) + bucket.prompt + bucket.completion,
    })
  }

  const cells: HeatmapCell[] = []
  for (let row = 0; row < WEEKDAYS.length; row++) {
    for (let hour = 0; hour < 24; hour++) {
      const slot = sums.get(`${row}:${hour}`)
      cells.push({
        row,
        column: hour,
        label: `${WEEKDAYS[row]} ${String(hour).padStart(2, "0")}:00`,
        value: slot ? slot.total : null,
        metrics: slot ? metricsFor(slot) : undefined,
      })
    }
  }

  // Every third hour is labelled; a label on all 24 collides at card width.
  const columnLabels = Array.from({ length: 24 }, (_, hour) =>
    hour % 3 === 0 ? String(hour).padStart(2, "0") : null,
  )
  return { rowLabels: WEEKDAYS, columnLabels, cells }
}
