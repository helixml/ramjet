// Building blocks shared by the Usage tab and the per-principal detail view.

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react"
import { ArrowDown, ArrowUp, ChevronDown, Pause, Play, RefreshCw } from "lucide-react"
import type { BucketRow, BucketSeries } from "@/components/BucketChart"
import { Segmented } from "@/components/Controls"
import { StatTile } from "@/components/StatTile"
import { Button } from "@/components/ui/button"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePoll } from "@/hooks/usePoll"
import {
  BUCKET_MS,
  SERVER_OTHER_KEY,
  UnauthorizedError,
  bucketFloor,
  errorRate,
  fetchRecentRequests,
  type Granularity,
  type RequestRecord,
  type SeriesResponse,
  type Stats,
  type StatsTotal,
  type UsageMeta,
} from "@/lib/accounting"
import {
  aggregateRows,
  groupLabel,
  groupRows,
  indexSeries,
  otherCount,
  plottedGroups,
  responseGrid,
  topFinishReasons,
  type Group,
  type Metric,
} from "@/lib/usage"
import {
  endpointLabel,
  fmtBytes,
  fmtClockFull,
  fmtCount,
  fmtDateTime,
  fmtMs,
  fmtNum,
  fmtPct,
  fmtRelative,
} from "@/lib/format"
import type { SparkPoint } from "@/lib/sparkline"
import { cn } from "@/lib/utils"

const HOUR = 3_600_000
const DAY = 86_400_000

export const RANGES: Record<Granularity, Array<{ value: string; label: string; ms: number }>> = {
  minute: [
    { value: "1h", label: "1h", ms: HOUR },
    { value: "6h", label: "6h", ms: 6 * HOUR },
    { value: "24h", label: "24h", ms: 24 * HOUR },
  ],
  hour: [
    { value: "24h", label: "24h", ms: 24 * HOUR },
    { value: "7d", label: "7d", ms: 7 * DAY },
    { value: "30d", label: "30d", ms: 30 * DAY },
  ],
  day: [
    { value: "7d", label: "7d", ms: 7 * DAY },
    { value: "30d", label: "30d", ms: 30 * DAY },
    { value: "90d", label: "90d", ms: 90 * DAY },
  ],
}

export const DEFAULT_RANGE: Record<Granularity, string> = { minute: "1h", hour: "24h", day: "30d" }

/** Window of whole buckets ending with the (partial) current one. */
export function windowFor(granularity: Granularity, rangeMs: number, tz: number) {
  const now = Date.now()
  const size = BUCKET_MS[granularity]
  const buckets = Math.max(1, Math.round(rangeMs / size))
  const from = bucketFloor(now, granularity, tz) - (buckets - 1) * size
  return { from_ms: from, to_ms: now }
}

export const pct = (value: number) => (value === 0 ? "0%" : fmtPct(value, value < 10 ? 1 : 0))
export const PCT_TICKS = [0, 25, 50, 75, 100]
export const ms = (value: number) => fmtMs(value)
export const tps = (value: number) => fmtNum(value, value < 10 ? 1 : 0)

export const METRICS = {
  requests: { kind: "count", get: (s) => s.requests },
  tokens: { kind: "count", get: (s) => s.prompt_tokens + s.completion_tokens },
  cacheHit: {
    kind: "ratio",
    num: (s) => s.cached_tokens,
    den: (s) => s.prompt_tokens,
    scale: 100,
  },
  errorRate: {
    kind: "ratio",
    num: (s) => s.client_errors + s.server_errors,
    den: (s) => s.requests,
    scale: 100,
  },
  ttftP50: { kind: "quantile", get: (s) => s.ttft_p50_ms },
  ttftP90: { kind: "quantile", get: (s) => s.ttft_p90_ms },
  tpsP50: { kind: "quantile", get: (s) => s.decode_tps_p50 },
  durationP90: { kind: "quantile", get: (s) => s.duration_p90_ms },
} satisfies Record<string, Metric>

export function spark(rows: BucketRow[], key: string): SparkPoint[] {
  return rows.map((row) => ({ t: row.t, v: row[key] ?? null }))
}

export type UsageView = ReturnType<typeof buildView>

export function buildView(response: SeriesResponse, meta: UsageMeta | null, tz: number) {
  const grid = responseGrid(response, tz)
  const index = indexSeries(response)
  const { groups, folded } = plottedGroups(response, meta)
  const rows = (metric: Metric) => groupRows(grid, index, groups, folded, metric)
  const overallRows = aggregateRows(grid, index, {
    requests: (s) => s.requests,
    prompt: (s) => s.prompt_tokens,
    cached: (s) => s.cached_tokens,
    uncached: (s) => Math.max(0, s.prompt_tokens - s.cached_tokens),
    completion: (s) => s.completion_tokens,
    client: (s) => s.client_errors,
    server: (s) => s.server_errors,
    disconnects: (s) => s.disconnects,
    errors: (s) => s.client_errors + s.server_errors,
    full: (s) => s.cache_full,
    partial: (s) => s.cache_partial,
    cold: (s) => s.cache_cold,
    unknown: (s) => s.cache_unknown,
  })
  for (const row of overallRows) {
    const prompt = row.prompt ?? 0
    row.hit = prompt > 0 ? ((row.cached ?? 0) / prompt) * 100 : null
  }
  const upstreams = meta?.upstreams ?? []
  const upstreamCount = Math.max(
    upstreams.length,
    ...response.totals.map((total) => total.upstream_requests.length),
    0,
  )
  const upstreamColumns: Record<string, (s: Stats) => number> = {}
  for (let i = 0; i < upstreamCount; i++) {
    upstreamColumns[`u${i}`] = (s) => s.upstream_requests[i] ?? 0
  }
  const upstreamRows = aggregateRows(grid, index, upstreamColumns)
  const upstreamSeries: BucketSeries[] = Array.from({ length: upstreamCount }, (_, i) => ({
    key: `u${i}`,
    label: upstreams[i] ? endpointLabel(upstreams[i], i) : `upstream ${i}`,
    // Same positional engine colors as the Serving tab (slot 2 = A, …).
    color: `var(--chart-${(i % 7) + 2})`,
  }))
  return { grid, groups, rows, overallRows, upstreamRows, upstreamSeries }
}


/** The range's headline numbers, with per-bucket sparklines where they add. */
export function KpiTiles({
  overall,
  overallRows,
  loading,
}: {
  overall: Stats | undefined
  overallRows: BucketRow[] | null
  loading: boolean
}) {
  const view = overallRows ? { overallRows } : null
  const rate = overall ? errorRate(overall) : null
  const served = overall ? overall.cache_full + overall.cache_partial + overall.cache_cold : 0
  const share = (n: number) => (served > 0 ? `${Math.round((n / served) * 100)}%` : "—")
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-3 lg:grid-cols-5">
      <StatTile
        label="Requests"
        value={fmtCount(overall?.requests)}
        detail={overall ? `${fmtCount(overall.streamed)} streamed` : undefined}
        trend={view ? spark(view.overallRows, "requests") : undefined}
        format={fmtCount}
        loading={loading}
      />
      <StatTile
        label="Prompt tokens"
        value={fmtCount(overall?.prompt_tokens)}
        detail={overall ? `${fmtCount(overall.cached_tokens)} cached` : undefined}
        trend={view ? spark(view.overallRows, "prompt") : undefined}
        format={fmtCount}
        loading={loading}
      />
      <StatTile
        label="Prompt / request"
        value={
          overall && overall.ok > 0 ? fmtCount(overall.prompt_tokens / overall.ok) : "—"
        }
        detail={
          overall && overall.ok > 0
            ? `${fmtCount(overall.completion_tokens / overall.ok)} completion / request`
            : undefined
        }
        loading={loading}
      />
      <StatTile
        label="Completion tokens"
        value={fmtCount(overall?.completion_tokens)}
        trend={view ? spark(view.overallRows, "completion") : undefined}
        format={fmtCount}
        loading={loading}
      />
      <StatTile
        label="Cache hit"
        value={overall?.cache_hit_ratio != null ? pct(overall.cache_hit_ratio * 100) : "—"}
        detail={overall ? `full ${share(overall.cache_full)} · cold ${share(overall.cache_cold)}` : undefined}
        trend={view ? spark(view.overallRows, "hit") : undefined}
        format={pct}
        loading={loading}
      />
      <StatTile
        label="Error rate"
        value={rate != null ? fmtPct(rate * 100, 2) : "—"}
        detail={
          overall
            ? `4xx ${fmtCount(overall.client_errors)} · 5xx ${fmtCount(overall.server_errors)}`
            : undefined
        }
        trend={view ? spark(view.overallRows, "errors") : undefined}
        format={fmtCount}
        loading={loading}
      />
      <StatTile
        label="TTFT p50"
        value={fmtMs(overall?.ttft_p50_ms)}
        detail={overall ? `p90 ${fmtMs(overall.ttft_p90_ms)}` : undefined}
        loading={loading}
      />
      <StatTile
        label="Decode tok/s p50"
        value={overall?.decode_tps_p50 != null ? tps(overall.decode_tps_p50) : "—"}
        detail={
          overall?.decode_tps_p10 != null ? `p10 ${tps(overall.decode_tps_p10)}` : undefined
        }
        loading={loading}
      />
      <StatTile
        label="Duration p50"
        value={fmtMs(overall?.duration_p50_ms)}
        detail={overall ? `p90 ${fmtMs(overall.duration_p90_ms)}` : undefined}
        loading={loading}
      />
      <StatTile
        label="Disconnects"
        value={fmtCount(overall?.disconnects)}
        detail={
          overall && overall.requests > 0
            ? `${fmtPct((overall.disconnects / overall.requests) * 100, 2)} of requests`
            : undefined
        }
        trend={view ? spark(view.overallRows, "disconnects") : undefined}
        format={fmtCount}
        loading={loading}
      />
    </div>
  )
}

/** Hash route of a principal's detail view. */
export function detailHref(id: string): string {
  return `#tokens/${encodeURIComponent(id)}`
}

/** A principal name that opens its detail view. */
export function PrincipalLink({
  id,
  children,
  className,
}: {
  id: string
  children: ReactNode
  className?: string
}) {
  return (
    <a
      href={detailHref(id)}
      onClick={(event) => event.stopPropagation()}
      className={cn("underline-offset-2 hover:underline focus-visible:underline outline-none", className)}
    >
      {children}
    </a>
  )
}

/** Granularity + range presets, shared by the Usage tab and detail view. */
export function RangeControls({
  granularity,
  range,
  onGranularity,
  onRange,
}: {
  granularity: Granularity
  range: string
  onGranularity: (granularity: Granularity) => void
  onRange: (range: string) => void
}) {
  return (
    <>
      <Segmented
        label="Granularity"
        value={granularity}
        onChange={(next) => {
          onGranularity(next)
          onRange(DEFAULT_RANGE[next])
        }}
        options={[
          { value: "minute", label: "minute" },
          { value: "hour", label: "hour" },
          { value: "day", label: "day" },
        ]}
      />
      <Segmented
        label="Range"
        value={range}
        onChange={onRange}
        options={RANGES[granularity].map(({ value, label }) => ({ value, label }))}
      />
    </>
  )
}

export function rangeMsOf(granularity: Granularity, range: string): number {
  return (RANGES[granularity].find((option) => option.value === range) ?? RANGES[granularity][0]).ms
}

/** Finish reasons as share bars, from one Stats. */
export function FinishReasonsCard({ stats, loading }: { stats: Stats | undefined; loading: boolean }) {
  const entries = Object.entries(stats?.finish_reasons ?? {})
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1])
  const total = entries.reduce((sum, [, n]) => sum + n, 0)
  const outcomes: Array<[string, number]> = stats
    ? [
        ["full", stats.cache_full],
        ["partial", stats.cache_partial],
        ["cold", stats.cache_cold],
        ["unknown", stats.cache_unknown],
      ]
    : []
  const outcomeTotal = outcomes.reduce((sum, [, n]) => sum + n, 0)
  const bars = (rows: Array<[string, number]>, sum: number) =>
    rows.map(([label, n]) => (
      <div key={label} className="flex items-center gap-2 text-[11px]">
        <span className="text-muted-foreground w-20 shrink-0 truncate font-mono">{label}</span>
        <div className="bg-muted h-1.5 flex-1 overflow-hidden rounded-full">
          <div className="bg-chart-1 h-full rounded-full" style={{ width: `${sum > 0 ? (n / sum) * 100 : 0}%` }} />
        </div>
        <span className="w-24 shrink-0 text-right font-mono tabular-nums">
          {fmtCount(n)} <span className="text-faint-foreground">{sum > 0 ? `${Math.round((n / sum) * 100)}%` : ""}</span>
        </span>
      </div>
    ))
  return (
    <Card aria-busy={loading || undefined}>
      <CardHeader>
        <CardTitle>Finish reasons &amp; cache outcomes</CardTitle>
        <CardDescription>over the selected window</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {loading ? (
          <Skeleton className="h-24 w-full" />
        ) : total === 0 && outcomeTotal === 0 ? (
          <div className="text-faint-foreground py-4 text-center text-xs">no completed requests in this window</div>
        ) : (
          <>
            <div className="flex flex-col gap-1.5">
              <div className="text-faint-foreground text-[10px] uppercase tracking-wider">finish reason</div>
              {bars(entries, total)}
            </div>
            <div className="flex flex-col gap-1.5">
              <div className="text-faint-foreground text-[10px] uppercase tracking-wider">prefix cache (requests)</div>
              {bars(outcomes, outcomeTotal)}
            </div>
          </>
        )}
      </CardContent>
    </Card>
  )
}

// ------------------------------------------------------------ breakdown

interface Column {
  id: string
  label: string
  title?: string
  value: (s: Stats) => number | null
  format: (v: number) => string
}

const COLUMNS: Column[] = [
  { id: "requests", label: "req", title: "requests", value: (s) => s.requests, format: (v) => fmtCount(v) },
  { id: "prompt", label: "prompt", title: "prompt tokens", value: (s) => s.prompt_tokens, format: (v) => fmtCount(v) },
  { id: "cached", label: "cached", title: "cached prompt tokens", value: (s) => s.cached_tokens, format: (v) => fmtCount(v) },
  { id: "completion", label: "compl", title: "completion tokens", value: (s) => s.completion_tokens, format: (v) => fmtCount(v) },
  {
    id: "hit",
    label: "hit",
    title: "cache hit ratio (cached ÷ prompt tokens)",
    value: (s) => (s.cache_hit_ratio == null ? null : s.cache_hit_ratio * 100),
    format: (v) => fmtPct(v, 1),
  },
  { id: "ttft50", label: "ttft p50", value: (s) => s.ttft_p50_ms, format: ms },
  { id: "ttft90", label: "p90", title: "TTFT p90", value: (s) => s.ttft_p90_ms, format: ms },
  { id: "ttft99", label: "p99", title: "TTFT p99", value: (s) => s.ttft_p99_ms, format: ms },
  { id: "dur50", label: "dur p50", title: "duration p50", value: (s) => s.duration_p50_ms, format: ms },
  { id: "dur90", label: "p90", title: "duration p90", value: (s) => s.duration_p90_ms, format: ms },
  { id: "tps10", label: "tok/s p10", title: "decode tok/s p10 (slow tail)", value: (s) => s.decode_tps_p10, format: tps },
  { id: "tps50", label: "p50", title: "decode tok/s p50", value: (s) => s.decode_tps_p50, format: tps },
  { id: "e4", label: "4xx", value: (s) => s.client_errors, format: (v) => fmtCount(v) },
  { id: "e5", label: "5xx", value: (s) => s.server_errors, format: (v) => fmtCount(v) },
  { id: "disc", label: "disc", title: "client disconnects", value: (s) => s.disconnects, format: (v) => fmtCount(v) },
]

export function BreakdownTable({
  response,
  meta,
  groups,
  loading,
  onSelectKey,
}: {
  response: SeriesResponse | null
  meta: UsageMeta | null
  groups: Group[]
  loading: boolean
  onSelectKey: (key: string) => void
}) {
  const [sort, setSort] = useState<{ id: string; desc: boolean }>({ id: "requests", desc: true })
  const groupBy = response?.group_by ?? "token"
  const colorOf = new Map(groups.map((group) => [group.key, group.color]))
  const upstreams = meta?.upstreams ?? []
  const rows = useMemo(() => {
    const totals = [...(response?.totals ?? [])]
    const column = COLUMNS.find((candidate) => candidate.id === sort.id)
    const value = (total: StatsTotal) =>
      column ? column.value(total) : null
    return totals.sort((a, b) => {
      // The server's exact `__other` fold is a remainder, not a peer: it
      // stays at the bottom whichever column is sorted.
      if (a.key === SERVER_OTHER_KEY || b.key === SERVER_OTHER_KEY) {
        return Number(a.key === SERVER_OTHER_KEY) - Number(b.key === SERVER_OTHER_KEY)
      }
      if (sort.id === "name") {
        const order = groupLabel(a.key, groupBy, meta).localeCompare(groupLabel(b.key, groupBy, meta))
        return sort.desc ? -order : order
      }
      const va = value(a)
      const vb = value(b)
      // Unknowns sink to the bottom whichever way the column is sorted.
      if (va == null && vb == null) return 0
      if (va == null) return 1
      if (vb == null) return -1
      return sort.desc ? vb - va : va - vb
    })
  }, [response, sort, groupBy, meta])
  const clickable = groupBy !== "none"
  const folded = response ? otherCount(response) : 0

  const header = (id: string, label: string, title?: string, align: "left" | "right" = "right") => (
    <th
      key={id}
      title={title}
      aria-sort={sort.id === id ? (sort.desc ? "descending" : "ascending") : undefined}
      className={cn(
        "text-faint-foreground px-2 py-1.5 font-medium whitespace-nowrap",
        align === "right" ? "text-right" : "text-left",
      )}
    >
      <button
        type="button"
        className="inline-flex items-center gap-0.5 hover:text-foreground"
        onClick={() =>
          setSort((current) =>
            current.id === id ? { id, desc: !current.desc } : { id, desc: id !== "name" },
          )
        }
      >
        {label}
        {sort.id === id ? (
          sort.desc ? <ArrowDown className="size-3" /> : <ArrowUp className="size-3" />
        ) : null}
      </button>
    </th>
  )

  return (
    <Card aria-busy={loading || undefined}>
      <CardHeader>
        <CardTitle>Breakdown</CardTitle>
        <CardDescription>
          {groupBy === "token"
            ? "one row per principal over the selected window · click a name for its detail, a row to filter to it"
            : groupBy === "model"
              ? "one row per model over the selected window · click a row to filter to it"
              : "all requests over the selected window"}
          {response?.group_count != null && groupBy === "token"
            ? ` · ${response.group_count.toLocaleString("en-US")} principals with traffic`
            : ""}
          {" · quantiles are histogram estimates"}
        </CardDescription>
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="h-32 w-full" />
        ) : rows.length === 0 ? (
          <div className="text-faint-foreground py-6 text-center text-xs">no requests in this window</div>
        ) : (
          <div className="overflow-x-auto rounded-md border border-border">
            <table className="w-full text-[11px] tabular-nums">
              <thead className="bg-muted/60">
                <tr>
                  {header("name", groupBy === "model" ? "model" : groupBy === "token" ? "principal" : "group", undefined, "left")}
                  {COLUMNS.map((column) => header(column.id, column.label, column.title))}
                  <th className="text-faint-foreground px-2 py-1.5 text-left font-medium whitespace-nowrap" title="requests by cache outcome">
                    full / partial / cold
                  </th>
                  <th className="text-faint-foreground px-2 py-1.5 text-left font-medium whitespace-nowrap">finish</th>
                  <th className="text-faint-foreground px-2 py-1.5 text-left font-medium whitespace-nowrap">upstreams</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((total) => {
                  const principal = meta?.principals.find((p) => p.key === total.key)
                  const outcomes = total.cache_full + total.cache_partial + total.cache_cold
                  const upstreamTotal = total.upstream_requests.reduce((a, b) => a + b, 0)
                  const isOther = total.key === SERVER_OTHER_KEY
                  const label = isOther
                    ? `other (${folded.toLocaleString("en-US")} more)`
                    : groupLabel(total.key, groupBy, meta)
                  return (
                    <tr
                      key={total.key}
                      className={cn(
                        "border-t border-border/60",
                        clickable && !isOther && "cursor-pointer hover:bg-muted/50",
                      )}
                      onClick={clickable && !isOther ? () => onSelectKey(total.key) : undefined}
                      title={clickable && !isOther ? `filter to ${label}` : undefined}
                    >
                      <td className="px-2 py-1 text-left">
                        <span className="flex items-center gap-1.5 whitespace-nowrap">
                          <span
                            aria-hidden
                            className="size-2 shrink-0 rounded-[2px]"
                            style={{ background: colorOf.get(total.key) ?? "var(--faint-foreground)" }}
                          />
                          {groupBy === "token" && !isOther ? (
                            <PrincipalLink id={total.key} className="font-medium">
                              {label}
                            </PrincipalLink>
                          ) : (
                            <span className={cn("font-medium", isOther && "text-muted-foreground")}>{label}</span>
                          )}
                          {principal && principal.kind !== "token" ? (
                            <span className="text-faint-foreground">{principal.kind.replace("_", " ")}</span>
                          ) : groupBy === "token" && principal?.kind === "token" ? (
                            <span className="text-faint-foreground font-mono">{total.key.slice(0, 6)}</span>
                          ) : null}
                        </span>
                      </td>
                      {COLUMNS.map((column) => {
                        const value = column.value(total)
                        const alarming =
                          (column.id === "e5" && (value ?? 0) > 0) ||
                          (column.id === "e4" && (value ?? 0) > 0 && total.key === "rejected")
                        return (
                          <td
                            key={column.id}
                            className={cn(
                              "px-2 py-1 text-right font-mono whitespace-nowrap",
                              value == null && "text-faint-foreground",
                              alarming && "font-semibold",
                            )}
                          >
                            {value == null ? "—" : column.format(value)}
                          </td>
                        )
                      })}
                      <td className="px-2 py-1 font-mono whitespace-nowrap">
                        {outcomes > 0 ? (
                          <span title={`full ${total.cache_full} · partial ${total.cache_partial} · cold ${total.cache_cold} · unknown ${total.cache_unknown}`}>
                            {[total.cache_full, total.cache_partial, total.cache_cold]
                              .map((n) => `${Math.round((n / outcomes) * 100)}`)
                              .join(" / ")}
                            %
                          </span>
                        ) : (
                          <span className="text-faint-foreground">—</span>
                        )}
                      </td>
                      <td className="text-muted-foreground px-2 py-1 whitespace-nowrap">{topFinishReasons(total)}</td>
                      <td className="px-2 py-1">
                        {upstreamTotal > 0 ? (
                          <UpstreamSplit counts={total.upstream_requests} upstreams={upstreams} />
                        ) : (
                          <span className="text-faint-foreground">—</span>
                        )}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}
      </CardContent>
    </Card>
  )
}

export function UpstreamSplit({ counts, upstreams }: { counts: number[]; upstreams: string[] }) {
  const total = counts.reduce((a, b) => a + b, 0)
  const label = (i: number) => (upstreams[i] ? endpointLabel(upstreams[i], i) : `upstream ${i}`)
  const title = counts
    .map((n, i) => `${label(i)}: ${n} (${Math.round((n / total) * 100)}%)`)
    .join("\n")
  return (
    <div className="flex min-w-[110px] items-center gap-1.5" title={title}>
      <div className="bg-muted flex h-1.5 w-16 gap-px overflow-hidden rounded-full">
        {counts.map((n, i) =>
          n > 0 ? (
            <div key={i} style={{ width: `${(n / total) * 100}%`, background: `var(--chart-${(i % 7) + 2})` }} />
          ) : null,
        )}
      </div>
      <span className="text-muted-foreground font-mono whitespace-nowrap">
        {counts
          .map((n, i) => (n > 0 ? `${String.fromCharCode(65 + i)} ${Math.round((n / total) * 100)}%` : null))
          .filter(Boolean)
          .join(" ")}
      </span>
    </div>
  )
}

// ------------------------------------------------------------ recent

export function statusColor(status: number): string | null {
  if (status >= 500) return "var(--status-critical)"
  if (status >= 400) return "var(--status-warning)"
  return null
}

export function isErrorRow(record: RequestRecord): boolean {
  return record.status >= 400 || (record.result !== "complete" && record.result !== "")
}

const PAGE = 200

/**
 * The newest page is polled; "load older" pages backwards with `before_ms`
 * set to the oldest row held. Older pages and a moving head cannot be
 * stitched without gaps, so loading an older page pauses the live head.
 */
export function RecentRequests({
  meta,
  tokenFilter,
  modelFilter,
  onUnauthorized,
  fixedToken = false,
}: {
  meta: UsageMeta | null
  tokenFilter: string
  modelFilter: string
  onUnauthorized: () => void
  /** Detail view: the principal is the page itself, so hide its column. */
  fixedToken?: boolean
}) {
  const [errorsOnly, setErrorsOnly] = useState(false)
  const [paused, setPaused] = useState(false)
  const [older, setOlder] = useState<RequestRecord[]>([])
  const [exhausted, setExhausted] = useState(false)
  const [loadingOlder, setLoadingOlder] = useState(false)
  const [olderError, setOlderError] = useState<string | null>(null)
  const key = [tokenFilter, modelFilter, errorsOnly].join("|")
  // A different query starts from a fresh head.
  const lastKey = useRef(key)
  useEffect(() => {
    if (lastKey.current === key) return
    lastKey.current = key
    setOlder([])
    setExhausted(false)
    setOlderError(null)
  }, [key])
  const recent = usePoll(
    () =>
      fetchRecentRequests({
        limit: PAGE,
        token: tokenFilter || undefined,
        model: modelFilter || undefined,
        errors_only: errorsOnly,
      }),
    key,
    paused ? 0 : 5_000,
    onUnauthorized,
  )
  const name = (key: string) => meta?.principals.find((p) => p.key === key)?.name ?? key
  const upstreams = meta?.upstreams ?? []
  const head = recent.data?.requests ?? []
  const requests = useMemo(() => [...head, ...older], [head, older])
  const now = Date.now()

  const loadOlder = async () => {
    const oldest = requests.at(-1)?.t_ms
    if (oldest == null || loadingOlder) return
    setPaused(true)
    setLoadingOlder(true)
    setOlderError(null)
    try {
      const page = await fetchRecentRequests({
        limit: PAGE,
        token: tokenFilter || undefined,
        model: modelFilter || undefined,
        errors_only: errorsOnly,
        before_ms: oldest,
      })
      setOlder((current) => [...current, ...page.requests])
      if (page.requests.length < PAGE) setExhausted(true)
    } catch (cause) {
      if (cause instanceof UnauthorizedError) onUnauthorized()
      else setOlderError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setLoadingOlder(false)
    }
  }
  const resume = () => {
    setOlder([])
    setExhausted(false)
    setPaused(false)
  }

  return (
    <Card aria-busy={recent.loading || undefined}>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-2">
        <div className="flex flex-col gap-1">
          <CardTitle>Recent requests</CardTitle>
          <CardDescription>
            newest first · {requests.length.toLocaleString("en-US")} shown
            {recent.data ? ` of a ${fmtCount(recent.data.capacity)}-request ring` : ""}
            {!fixedToken && (tokenFilter || modelFilter) ? " · filtered like the charts above" : ""}
            {" · prompts and responses are never recorded"}
          </CardDescription>
        </div>
        <div className="flex items-center gap-2">
          <label className="text-muted-foreground flex items-center gap-1.5 text-[11px]">
            <input
              type="checkbox"
              checked={errorsOnly}
              onChange={(event) => setErrorsOnly(event.target.checked)}
              className="accent-[var(--primary)]"
            />
            errors only
          </label>
          <Button
            variant="outline"
            onClick={() => (paused ? resume() : setPaused(true))}
            aria-label={paused ? "Resume auto-refresh" : "Pause auto-refresh"}
            title={older.length > 0 ? "resuming drops the older pages and jumps to the newest" : undefined}
          >
            {paused ? <Play /> : <Pause />}
            {paused ? (older.length > 0 ? "paused · browsing older" : "paused") : "live · 5 s"}
          </Button>
          <Button variant="ghost" size="icon" aria-label="Refresh now" onClick={recent.refresh}>
            <RefreshCw />
          </Button>
        </div>
      </CardHeader>
      <CardContent>
        {recent.error ? (
          <div className="mb-2 text-xs" style={{ color: "var(--status-critical)" }}>
            ⚠ {recent.error}
          </div>
        ) : null}
        {recent.loading && recent.data == null ? (
          <Skeleton className="h-40 w-full" />
        ) : requests.length === 0 ? (
          <div className="text-faint-foreground py-6 text-center text-xs">
            {errorsOnly ? "no failed requests in the ring" : "no requests recorded yet"}
          </div>
        ) : (
          <div className="max-h-[520px] overflow-auto rounded-md border border-border">
            <table className="w-full text-[11px] tabular-nums">
              <thead className="bg-muted sticky top-0 z-10">
                <tr className="text-faint-foreground">
                  {[
                    ["time", "left"],
                    ...(fixedToken ? [] : [["principal", "left"]]),
                    ["model", "left"],
                    ["endpoint", "left"],
                    ["upstream", "left"],
                    ["status", "left"],
                    ["result", "left"],
                    ["prompt", "right"],
                    ["cached", "right"],
                    ["compl", "right"],
                    ["cache", "left"],
                    ["ttft", "right"],
                    ["duration", "right"],
                    ["tok/s", "right"],
                    ["finish", "left"],
                    ["route", "left"],
                    ["bytes in/out", "right"],
                  ].map(([label, align]) => (
                    <th
                      key={label}
                      className={cn(
                        "px-2 py-1.5 font-medium whitespace-nowrap",
                        align === "right" ? "text-right" : "text-left",
                      )}
                    >
                      {label}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody className="font-mono">
                {requests.map((record, index) => {
                  const error = isErrorRow(record)
                  const color = statusColor(record.status)
                  const severe = record.status >= 500 || record.result === "upstream_error" || record.result === "upstream_read_error"
                  return (
                    <tr
                      key={`${record.t_ms}-${index}`}
                      className="border-t border-border/60"
                      style={
                        error
                          ? {
                              background: `color-mix(in srgb, ${
                                severe ? "var(--status-critical)" : "var(--status-warning)"
                              } 8%, transparent)`,
                            }
                          : undefined
                      }
                    >
                      <td className="px-2 py-0.5 whitespace-nowrap" title={fmtDateTime(record.t_ms)}>
                        {fmtClockFull(record.t_ms)}
                        <span className="text-faint-foreground ml-1">{fmtRelative(record.t_ms, now)}</span>
                      </td>
                      {fixedToken ? null : (
                        <td className="max-w-[140px] truncate px-2 py-0.5 font-sans" title={record.principal}>
                          <PrincipalLink id={record.principal}>{name(record.principal)}</PrincipalLink>
                        </td>
                      )}
                      <td className="max-w-[160px] truncate px-2 py-0.5" title={record.model}>
                        {record.model || "—"}
                      </td>
                      <td className="px-2 py-0.5 whitespace-nowrap" title={record.stream ? "streamed" : "not streamed"}>
                        {record.endpoint}
                        {record.stream ? <span className="text-faint-foreground"> ·s</span> : null}
                      </td>
                      <td className="px-2 py-0.5 whitespace-nowrap">
                        {record.upstream == null
                          ? "—"
                          : upstreams[record.upstream]
                            ? endpointLabel(upstreams[record.upstream], record.upstream)
                            : `#${record.upstream}`}
                      </td>
                      <td className="px-2 py-0.5 whitespace-nowrap">
                        <span className="inline-flex items-center gap-1">
                          <span
                            aria-hidden
                            className="size-1.5 rounded-full"
                            style={{ background: color ?? "var(--status-good)" }}
                          />
                          <span className={color ? "font-semibold" : undefined}>{record.status}</span>
                        </span>
                      </td>
                      <td
                        className={cn(
                          "px-2 py-0.5 whitespace-nowrap",
                          record.result === "complete" ? "text-muted-foreground" : "font-semibold",
                        )}
                      >
                        {record.result.replace(/_/g, " ")}
                      </td>
                      <td className="px-2 py-0.5 text-right">{fmtCount(record.prompt_tokens)}</td>
                      <td className="px-2 py-0.5 text-right">{fmtCount(record.cached_tokens)}</td>
                      <td className="px-2 py-0.5 text-right">{fmtCount(record.completion_tokens)}</td>
                      <td className="text-muted-foreground px-2 py-0.5">{record.cache_outcome}</td>
                      <td className="px-2 py-0.5 text-right">{fmtMs(record.ttft_ms)}</td>
                      <td className="px-2 py-0.5 text-right">{fmtMs(record.duration_ms)}</td>
                      <td className="px-2 py-0.5 text-right">
                        {record.decode_tps == null ? "—" : tps(record.decode_tps)}
                      </td>
                      <td className="text-muted-foreground px-2 py-0.5">{record.finish_reason ?? "—"}</td>
                      <td className="text-muted-foreground px-2 py-0.5">{record.route_outcome ?? "—"}</td>
                      <td className="text-muted-foreground px-2 py-0.5 text-right whitespace-nowrap">
                        {fmtBytes(record.request_bytes)} / {fmtBytes(record.response_bytes)}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
            <div className="flex items-center justify-center gap-3 border-t border-border/60 py-2 font-sans">
              {exhausted ? (
                <span className="text-faint-foreground text-[11px]">reached the oldest retained request</span>
              ) : (
                <Button variant="outline" disabled={loadingOlder} onClick={() => void loadOlder()}>
                  <ChevronDown />
                  {loadingOlder ? "loading…" : `load ${PAGE} older`}
                </Button>
              )}
              {olderError ? (
                <span className="text-[11px]" style={{ color: "var(--status-critical)" }}>
                  ⚠ {olderError}
                </span>
              ) : null}
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  )
}
