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

/**
 * Calendar grid: one column per week, one row per weekday.
 *
 * Days inside the covered span with no recorded hour keep a `null` value —
 * an LB that was down is not a day of zero traffic, and the two must not
 * look the same.
 */
export function dayGrid(buckets: TokenBucket[], now: number): Grid {
  if (buckets.length === 0) {
    return { rowLabels: WEEKDAYS, columnLabels: [], cells: [] }
  }
  const byDay = new Map<number, TokenTotals>()
  for (const bucket of buckets) {
    const day = startOfLocalDay(bucket.t)
    const current = byDay.get(day) ?? totals([])
    byDay.set(day, {
      prompt: current.prompt + bucket.prompt,
      completion: current.completion + bucket.completion,
      cached: current.cached + bucket.cached,
      requests: current.requests + bucket.requests,
      total: current.total + bucket.prompt + bucket.completion,
    })
  }

  const firstDay = startOfLocalDay(Math.min(...buckets.map((bucket) => bucket.t)))
  const lastDay = startOfLocalDay(now)
  // Pad back to the Monday of the first week so rows are true weekdays.
  const firstMonday = new Date(firstDay)
  firstMonday.setDate(firstMonday.getDate() - weekdayIndex(firstMonday))
  const origin = firstMonday.getTime()
  const columns = Math.floor(daysBetween(origin, lastDay) / 7) + 1

  const cells: HeatmapCell[] = []
  for (let column = 0; column < columns; column++) {
    for (let row = 0; row < WEEKDAYS.length; row++) {
      const date = new Date(origin)
      date.setDate(date.getDate() + column * 7 + row)
      const day = date.getTime()
      if (day < firstDay || day > lastDay) continue
      const dayTotals = byDay.get(day)
      cells.push({
        row,
        column,
        label: date.toLocaleDateString("en-GB", {
          weekday: "short",
          day: "numeric",
          month: "short",
        }),
        value: dayTotals ? dayTotals.total : null,
        metrics: dayTotals ? metricsFor(dayTotals) : undefined,
      })
    }
  }

  // One month label per column, printed only where the month changes.
  let previousMonth = ""
  const columnLabels = Array.from({ length: columns }, (_, column) => {
    const date = new Date(origin)
    date.setDate(date.getDate() + column * 7)
    const month = date.toLocaleDateString("en-GB", { month: "short" })
    if (month === previousMonth) return null
    previousMonth = month
    return month
  })

  return { rowLabels: WEEKDAYS, columnLabels, cells }
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
