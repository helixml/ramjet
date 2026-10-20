// Turns a sparse /api/usage/series response into dense chart rows.

import {
  SERVER_OTHER_KEY,
  bucketGrid,
  type GroupBy,
  type SeriesResponse,
  type Stats,
  type UsageMeta,
} from "@/lib/accounting"
import type { BucketRow, BucketSeries } from "@/components/BucketChart"

/** Categorical slots available; a ninth group folds into "other". */
const PALETTE_SLOTS = 8
/**
 * Client-side fold, used only when the response still has more groups than
 * colors (e.g. many models). Distinct from the server's exact `__other`.
 */
export const OTHER_KEY = "__fold"
export { SERVER_OTHER_KEY }
export const OTHER_COLOR = "var(--faint-foreground)"

/** How many groups the server folded into `__other`, when it did. */
export function otherCount(response: SeriesResponse): number {
  const returned = response.totals.filter((total) => total.key !== SERVER_OTHER_KEY).length
  return Math.max(0, (response.group_count ?? returned) - returned)
}

export interface Group {
  key: string
  label: string
  color: string
}

/** Display name for a group key, given the current grouping. */
export function groupLabel(key: string, groupBy: GroupBy, meta: UsageMeta | null): string {
  if (key === SERVER_OTHER_KEY) return "other"
  if (groupBy === "none") return "all requests"
  if (groupBy === "model") return key
  const principal = meta?.principals.find((candidate) => candidate.key === key)
  return principal?.name ?? key
}

function hashKey(key: string): number {
  let h = 2166136261
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i)
    h = Math.imul(h, 16777619)
  }
  return h >>> 0
}

/** Fixed slots for the well-known principals, so they never move. */
const FIXED_SLOTS: Record<string, number> = { master: 1, anonymous: 6, rejected: 7 }

/**
 * Color follows the entity, never its rank or its position in a list: each
 * key prefers a slot derived from the key itself, so minting a token,
 * filtering, or a new busiest group never repaints the others. Collisions
 * probe forward in a canonical order, so the same pair always resolves the
 * same way.
 */
export function stableColors(keys: string[]): Map<string, string> {
  const preferred = (key: string) => FIXED_SLOTS[key] ?? hashKey(key) % PALETTE_SLOTS
  const canonical = [...keys].sort(
    (a, b) =>
      Number(b in FIXED_SLOTS) - Number(a in FIXED_SLOTS) ||
      hashKey(a) - hashKey(b) ||
      a.localeCompare(b),
  )
  const taken = new Set<number>()
  const colors = new Map<string, string>()
  for (const key of canonical) {
    let slot = preferred(key)
    for (let tries = 0; taken.has(slot) && tries < PALETTE_SLOTS; tries++) {
      slot = (slot + 1) % PALETTE_SLOTS
    }
    taken.add(slot)
    colors.set(key, `var(--chart-${slot + 1})`)
  }
  return colors
}

/**
 * The plotted groups: every key when there are at most eight, otherwise the
 * seven busiest plus an "other" fold. Legend order is the stable meta order.
 */
export function plottedGroups(
  response: SeriesResponse | null,
  meta: UsageMeta | null,
): { groups: Group[]; folded: Set<string> } {
  if (!response) return { groups: [], folded: new Set() }
  const groupBy = response.group_by
  // The server's `__other` is an exact group of its own; it always plots
  // last, in the neutral color, and is never re-folded.
  const serverOther = response.totals.find((total) => total.key === SERVER_OTHER_KEY)
  const named = response.totals
    .filter((total) => total.key !== SERVER_OTHER_KEY)
    .sort((a, b) => b.requests - a.requests)
  const room = PALETTE_SLOTS - (serverOther ? 1 : 0)
  const shown = named.length > room ? named.slice(0, room - 1) : named
  const folded = new Set(named.slice(shown.length).map((total) => total.key))
  const keys = shown.map((total) => total.key)
  const colors = stableColors(keys)
  const order = groupBy === "token" ? (meta?.principals ?? []).map((p) => p.key) : meta?.models ?? []
  const rank = (key: string) => {
    const index = order.indexOf(key)
    return index < 0 ? Number.MAX_SAFE_INTEGER : index
  }
  const groups = keys
    .sort((a, b) => rank(a) - rank(b) || a.localeCompare(b))
    .map((key) => ({
      key,
      label: groupLabel(key, groupBy, meta),
      color: colors.get(key) ?? OTHER_COLOR,
    }))
  if (folded.size > 0) {
    groups.push({ key: OTHER_KEY, label: `more (${folded.size})`, color: "var(--axis)" })
  }
  if (serverOther) {
    groups.push({
      key: SERVER_OTHER_KEY,
      label: `other (${fmtOther(otherCount(response))})`,
      color: OTHER_COLOR,
    })
  }
  return { groups, folded }
}

function fmtOther(n: number): string {
  return n.toLocaleString("en-US")
}

/** Bucket starts covering the response window, plus any off-grid point. */
export function responseGrid(response: SeriesResponse, tzOffsetMin: number): number[] {
  const extra: number[] = []
  for (const entry of response.series) for (const point of entry.points) extra.push(point.t)
  return bucketGrid(response.from_ms, response.to_ms, response.granularity, tzOffsetMin, extra)
}

/** Per-group lookup: group key -> bucket start -> stats. */
export function indexSeries(response: SeriesResponse): Map<string, Map<number, Stats>> {
  const index = new Map<string, Map<number, Stats>>()
  for (const entry of response.series) {
    index.set(entry.key, new Map(entry.points.map((point) => [point.t, point])))
  }
  return index
}

/** How a metric combines when several groups fold into "other". */
export type Metric =
  | { kind: "count"; get: (stats: Stats) => number }
  | { kind: "ratio"; num: (stats: Stats) => number; den: (stats: Stats) => number; scale?: number }
  | { kind: "quantile"; get: (stats: Stats) => number | null }

function valueOf(metric: Metric, stats: Stats | undefined): number | null {
  if (metric.kind === "count") return stats ? metric.get(stats) : 0
  if (!stats) return null
  if (metric.kind === "ratio") {
    const den = metric.den(stats)
    return den > 0 ? (metric.num(stats) / den) * (metric.scale ?? 1) : null
  }
  return metric.get(stats)
}

/**
 * One row per bucket, one column per plotted group. Counts are zero-filled
 * (a missing bucket had no requests); ratios and quantiles stay null (an
 * idle bucket has no latency, not a zero latency). "other" is summed for
 * counts and ratio parts, and omitted for quantiles, which cannot be merged.
 */
export function groupRows(
  grid: number[],
  index: Map<string, Map<number, Stats>>,
  groups: Group[],
  folded: Set<string>,
  metric: Metric,
): BucketRow[] {
  return grid.map((t) => {
    const row: BucketRow = { t }
    for (const group of groups) {
      if (group.key !== OTHER_KEY) {
        row[group.key] = valueOf(metric, index.get(group.key)?.get(t))
        continue
      }
      if (metric.kind === "quantile") {
        row[group.key] = null
        continue
      }
      let num = 0
      let den = 0
      for (const key of folded) {
        const stats = index.get(key)?.get(t)
        if (!stats) continue
        if (metric.kind === "count") num += metric.get(stats)
        else {
          num += metric.num(stats)
          den += metric.den(stats)
        }
      }
      row[group.key] =
        metric.kind === "count" ? num : den > 0 ? (num / den) * (metric.scale ?? 1) : null
    }
    return row
  })
}

/** Plotted series for groups that carry this metric. */
export function seriesFor(groups: Group[], metric: Metric): BucketSeries[] {
  return groups
    .filter((group) => !(group.key === OTHER_KEY && metric.kind === "quantile"))
    .map((group) => ({ key: group.key, label: group.label, color: group.color }))
}

/** Sum of a set of count fields across every group, per bucket. */
export function aggregateRows(
  grid: number[],
  index: Map<string, Map<number, Stats>>,
  columns: Record<string, (stats: Stats) => number>,
): BucketRow[] {
  return grid.map((t) => {
    const row: BucketRow = { t }
    for (const key of Object.keys(columns)) row[key] = 0
    for (const points of index.values()) {
      const stats = points.get(t)
      if (!stats) continue
      for (const [key, get] of Object.entries(columns)) {
        row[key] = (row[key] ?? 0) + get(stats)
      }
    }
    return row
  })
}

/** Top finish reasons as "stop 71% · tool_calls 26%". */
export function topFinishReasons(stats: Stats, limit = 3): string {
  const entries = Object.entries(stats.finish_reasons).filter(([, n]) => n > 0)
  const total = entries.reduce((sum, [, n]) => sum + n, 0)
  if (total === 0) return "—"
  return entries
    .sort((a, b) => b[1] - a[1])
    .slice(0, limit)
    .map(([reason, n]) => `${reason} ${Math.round((n / total) * 100)}%`)
    .join(" · ")
}
