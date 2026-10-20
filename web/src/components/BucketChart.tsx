import { useState, type ReactElement } from "react"
import { ChartArea, Table2 } from "lucide-react"
import {
  Area,
  Bar,
  CartesianGrid,
  ComposedChart,
  Line,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import type { Granularity } from "@/lib/accounting"

/**
 * Bucketed usage charts. Unlike TimeChart (a sampled time series that is
 * downsampled per pixel), these plot one mark per accounting bucket on a
 * category axis, so bars and the crosshair land exactly on a bucket.
 */

export type BucketRow = { t: number } & Record<string, number | null>

export interface BucketSeries {
  key: string
  label: string
  /** Marks only — text stays in ink. */
  color: string
}

export interface BucketChartProps {
  title: string
  description?: string
  data: BucketRow[]
  series: BucketSeries[]
  kind: "bar" | "line"
  granularity: Granularity
  format: (value: number) => string
  domain?: [number | "auto", number | "auto"]
  /** Explicit Y ticks, e.g. [0, 25, 50, 75, 100] for percentages. */
  ticks?: number[]
  height?: number
  loading?: boolean
  /** Show a summed "total" row in the tooltip (stacked counts). */
  showTotal?: boolean
}

const WEEKDAY: Intl.DateTimeFormatOptions = { weekday: "short" }

export function fmtBucketTick(t: number, granularity: Granularity, spanMs: number): string {
  const date = new Date(t)
  if (granularity === "day") {
    return date.toLocaleDateString("en-GB", { day: "numeric", month: "short" })
  }
  const time = date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" })
  if (spanMs > 36 * 3_600_000) {
    return `${date.toLocaleDateString("en-GB", { day: "numeric", month: "short" })} ${time}`
  }
  return time
}

export function fmtBucketLabel(t: number, granularity: Granularity): string {
  const date = new Date(t)
  const day = `${date.toLocaleDateString("en-GB", WEEKDAY)} ${date.toLocaleDateString("en-GB", {
    day: "numeric",
    month: "short",
  })}`
  if (granularity === "day") return day
  const time = date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" })
  if (granularity === "hour") {
    const end = new Date(t + 3_600_000).toLocaleTimeString("en-GB", {
      hour: "2-digit",
      minute: "2-digit",
    })
    return `${day}, ${time}–${end}`
  }
  return `${day}, ${time}`
}

interface TooltipEntry {
  dataKey?: string | number
  value?: number | string | Array<number | string> | null
}

function BucketTooltip({
  active,
  payload,
  label,
  series,
  granularity,
  format,
  kind,
  showTotal,
}: {
  active?: boolean
  payload?: TooltipEntry[]
  label?: number
  series: BucketSeries[]
  granularity: Granularity
  format: (value: number) => string
  kind: "bar" | "line"
  showTotal?: boolean
}) {
  if (!active || !payload?.length || typeof label !== "number") return null
  const byKey = new Map(payload.map((entry) => [String(entry.dataKey), entry.value]))
  // Stacks read top-down, so list them in the order they are drawn from the top.
  const ordered = kind === "bar" ? [...series].reverse() : series
  const many = ordered.length > 6
  let total = 0
  const rows = ordered
    .map((def) => {
      const raw = byKey.get(def.key)
      const value = typeof raw === "number" ? raw : null
      if (value != null) total += value
      return { def, value }
    })
    // Dense tooltips drop series with nothing to say in this bucket.
    .filter((row) => !many || (row.value != null && row.value !== 0))
  return (
    <div className="max-w-[280px] rounded-lg border border-border bg-card px-2.5 py-2 shadow-md">
      <div className="text-faint-foreground mb-1 text-[10px] tabular-nums">
        {fmtBucketLabel(label, granularity)}
      </div>
      <div className="flex flex-col gap-1">
        {rows.map(({ def, value }) => (
          <div key={def.key} className="flex items-center gap-2 text-xs">
            <span
              aria-hidden
              className="h-0.5 w-3 shrink-0 rounded-full"
              style={{ background: def.color }}
            />
            <span className="font-mono font-medium tabular-nums">
              {value == null ? "—" : format(value)}
            </span>
            <span className="text-muted-foreground truncate">{def.label}</span>
          </div>
        ))}
        {rows.length === 0 ? (
          <div className="text-muted-foreground text-xs">no requests</div>
        ) : null}
        {showTotal && series.length > 1 ? (
          <div className="mt-0.5 flex items-center gap-2 border-t border-border pt-1 text-xs">
            <span aria-hidden className="w-3 shrink-0" />
            <span className="font-mono font-medium tabular-nums">{format(total)}</span>
            <span className="text-muted-foreground">total</span>
          </div>
        ) : null}
      </div>
    </div>
  )
}

function Legend({ series, kind }: { series: BucketSeries[]; kind: "bar" | "line" }) {
  if (series.length < 2) return null
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      {series.map((def) => (
        <span
          key={def.key}
          className="text-muted-foreground flex items-center gap-1.5 text-[11px]"
        >
          <span
            aria-hidden
            className={kind === "bar" ? "size-2 rounded-[2px]" : "h-0.5 w-3 rounded-full"}
            style={{ background: def.color }}
          />
          {def.label}
        </span>
      ))}
    </div>
  )
}

function TableView({
  data,
  series,
  granularity,
  format,
}: Pick<BucketChartProps, "data" | "series" | "granularity" | "format">) {
  const rows = [...data].reverse()
  return (
    <div className="max-h-[200px] overflow-auto rounded-md border border-border">
      <table className="w-full font-mono text-[11px] tabular-nums">
        <thead className="bg-muted/60 sticky top-0">
          <tr>
            <th className="text-faint-foreground px-2 py-1 text-left font-medium">bucket</th>
            {series.map((def) => (
              <th
                key={def.key}
                className="text-faint-foreground px-2 py-1 text-right font-medium whitespace-nowrap"
              >
                {def.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.t} className="border-t border-border/60">
              <td className="text-muted-foreground px-2 py-0.5 whitespace-nowrap">
                {fmtBucketLabel(row.t, granularity)}
              </td>
              {series.map((def) => {
                const value = row[def.key]
                return (
                  <td key={def.key} className="px-2 py-0.5 text-right">
                    {typeof value === "number" ? format(value) : "—"}
                  </td>
                )
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

interface DotProps {
  cx?: number
  cy?: number
  index?: number
  dataKey?: string
}

export function BucketChart({
  title,
  description,
  data,
  series,
  kind,
  granularity,
  format,
  domain = [0, "auto"],
  ticks,
  height = 180,
  loading = false,
  showTotal = kind === "bar",
}: BucketChartProps) {
  const [table, setTable] = useState(false)
  const hasData = data.some((row) =>
    series.some((def) => typeof row[def.key] === "number" && row[def.key] !== 0),
  )
  const spanMs = data.length > 1 ? data[data.length - 1].t - data[0].t : 0
  const topKey = series.at(-1)?.key
  // Past a few hundred buckets a bar is narrower than its own gap and the
  // SVG holds tens of thousands of rects; draw the same stack as a stepped
  // area instead, which reads identically at that density.
  const dense = kind === "bar" && data.length > 360

  // A quantile series is null wherever a bucket had no requests. Lines skip
  // those gaps; a bucket with no neighbours would then vanish entirely, so
  // isolated points get a marker of their own.
  const isolatedDot = (props: DotProps): ReactElement<SVGElement> => {
    const { cx, cy, index, dataKey } = props
    const key = `${dataKey}-${index}`
    if (cx == null || cy == null || index == null || dataKey == null) return <g key={key} />
    const prev = data[index - 1]?.[dataKey]
    const next = data[index + 1]?.[dataKey]
    if (typeof prev === "number" || typeof next === "number") return <g key={key} />
    const color = series.find((def) => def.key === dataKey)?.color
    return (
      <circle key={key} cx={cx} cy={cy} r={3} fill={color} stroke="var(--card)" strokeWidth={1.5} />
    )
  }

  return (
    <Card aria-busy={loading || undefined}>
      <CardHeader className="flex-row items-start justify-between gap-2">
        <div className="flex min-w-0 flex-col gap-1">
          <CardTitle>{title}</CardTitle>
          {loading ? (
            <Skeleton className="h-3 w-40" />
          ) : description ? (
            <CardDescription>{description}</CardDescription>
          ) : null}
          {loading ? null : <Legend series={series} kind={kind} />}
        </div>
        <Button
          variant="ghost"
          size="icon"
          aria-label={table ? "Show chart" : "Show table"}
          onClick={() => setTable((current) => !current)}
          disabled={loading}
        >
          {table ? <ChartArea /> : <Table2 />}
        </Button>
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="w-full rounded-md" style={{ height }} />
        ) : !hasData ? (
          <div
            className="text-faint-foreground flex items-center justify-center text-xs"
            style={{ height }}
          >
            no requests in this window
          </div>
        ) : table ? (
          <TableView data={data} series={series} granularity={granularity} format={format} />
        ) : (
          <div style={{ height }} className="w-full">
            <ResponsiveContainer width="100%" height="100%">
              <ComposedChart
                data={data}
                margin={{ top: 10, right: 6, bottom: 0, left: 0 }}
                barCategoryGap="18%"
              >
                <CartesianGrid vertical={false} stroke="var(--grid)" strokeWidth={1} />
                <XAxis
                  dataKey="t"
                  tickFormatter={(t: number) => fmtBucketTick(t, granularity, spanMs)}
                  tickLine={false}
                  axisLine={{ stroke: "var(--axis)", strokeWidth: 1 }}
                  tick={{ fill: "var(--faint-foreground)", fontSize: 10 }}
                  tickMargin={6}
                  minTickGap={40}
                  height={22}
                />
                <YAxis
                  width={50}
                  domain={domain}
                  tickFormatter={(value: number) => format(value)}
                  tickLine={false}
                  axisLine={false}
                  tick={{ fill: "var(--faint-foreground)", fontSize: 10 }}
                  tickCount={4}
                  ticks={ticks}
                  allowDecimals={kind !== "bar"}
                />
                <Tooltip
                  cursor={
                    kind === "bar" && !dense
                      ? { fill: "var(--muted)", fillOpacity: 0.6 }
                      : { stroke: "var(--axis)", strokeWidth: 1 }
                  }
                  isAnimationActive={false}
                  content={
                    <BucketTooltip
                      series={series}
                      granularity={granularity}
                      format={format}
                      kind={kind}
                      showTotal={showTotal}
                    />
                  }
                />
                {dense
                  ? series.map((def) => (
                      <Area
                        key={def.key}
                        dataKey={def.key}
                        stackId="stack"
                        type="step"
                        stroke="none"
                        fill={def.color}
                        fillOpacity={0.85}
                        dot={false}
                        activeDot={false}
                        isAnimationActive={false}
                      />
                    ))
                  : kind === "bar"
                  ? series.map((def) => (
                      <Bar
                        key={def.key}
                        dataKey={def.key}
                        stackId="stack"
                        fill={def.color}
                        // A surface-colored edge is the 2px gap between
                        // stacked segments, not an outline.
                        stroke="var(--card)"
                        strokeWidth={data.length > 120 ? 0 : 1}
                        maxBarSize={24}
                        radius={def.key === topKey ? [3, 3, 0, 0] : 0}
                        isAnimationActive={false}
                      />
                    ))
                  : series.map((def) => (
                      <Line
                        key={def.key}
                        dataKey={def.key}
                        type="monotone"
                        stroke={def.color}
                        strokeWidth={2}
                        strokeLinejoin="round"
                        strokeLinecap="round"
                        dot={isolatedDot}
                        activeDot={{ r: 4, fill: def.color, stroke: "var(--card)", strokeWidth: 2 }}
                        connectNulls={false}
                        isAnimationActive={false}
                      />
                    ))}
              </ComposedChart>
            </ResponsiveContainer>
          </div>
        )}
      </CardContent>
    </Card>
  )
}
