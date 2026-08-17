import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import { fmtClock, fmtClockFull } from "@/lib/format"

export interface SeriesDef {
  key: string
  label: string
  /** CSS color, normally `var(--chart-N)`. Marks only — text stays in ink. */
  color: string
}

export interface BandDef {
  lowKey: string
  highKey: string
  label: string
  color: string
}

export interface TimeChartProps {
  data: Array<{ t: number } & Record<string, number | null>>
  series: SeriesDef[]
  rangeSeconds: number
  format: (value: number) => string
  stacked?: boolean
  /** Optional low–high envelope drawn behind the series (e.g. min–max). */
  band?: BandDef
  /** Fix the Y domain, e.g. [0, 100] for percentages. */
  domain?: [number | "auto", number | "auto"]
  /** Plot height in px, or "fill" to take the card's remaining height. */
  height?: number | "fill"
}

interface TooltipRow {
  dataKey?: string | number
  value?: number | string | Array<number | string> | null
  payload?: Record<string, number | null>
}

function ChartTooltip({
  active,
  payload,
  label,
  series,
  band,
  format,
}: {
  active?: boolean
  payload?: TooltipRow[]
  label?: number
  series: SeriesDef[]
  band?: BandDef
  format: (value: number) => string
}) {
  if (!active || !payload?.length || typeof label !== "number") return null
  const byKey = new Map(payload.map((row) => [String(row.dataKey), row.value]))
  const row = payload[0]?.payload
  const low = band ? row?.[band.lowKey] : null
  const high = band ? row?.[band.highKey] : null
  return (
    <div className="rounded-lg border border-border bg-card px-2.5 py-2 shadow-md">
      <div className="text-faint-foreground mb-1 text-[10px] tabular-nums">
        {fmtClockFull(label)}
      </div>
      <div className="flex flex-col gap-1">
        {series.map((def) => {
          const raw = byKey.get(def.key)
          const value = typeof raw === "number" ? format(raw) : "—"
          return (
            <div key={def.key} className="flex items-center gap-2 text-xs">
              <span
                aria-hidden
                className="h-0.5 w-3 shrink-0 rounded-full"
                style={{ background: def.color }}
              />
              <span className="font-medium tabular-nums">{value}</span>
              <span className="text-muted-foreground">{def.label}</span>
            </div>
          )
        })}
        {band && typeof low === "number" && typeof high === "number" ? (
          <div className="flex items-center gap-2 text-xs">
            <span
              aria-hidden
              className="h-2 w-3 shrink-0 rounded-sm opacity-40"
              style={{ background: band.color }}
            />
            <span className="font-medium tabular-nums">
              {format(low)} – {format(high)}
            </span>
            <span className="text-muted-foreground">{band.label}</span>
          </div>
        ) : null}
      </div>
    </div>
  )
}

export function TimeChart({
  data,
  series,
  rangeSeconds,
  format,
  stacked = false,
  band,
  domain = [0, "auto"],
  height = 180,
}: TimeChartProps) {
  const fillOpacity = stacked ? 0.45 : series.length === 1 ? 0.4 : 0
  return (
    <div
      style={height === "fill" ? undefined : { height }}
      className={height === "fill" ? "h-full min-h-[240px] w-full" : "w-full"}
    >
      <ResponsiveContainer width="100%" height="100%">
        <AreaChart
          data={data}
          margin={{ top: 10, right: 6, bottom: 0, left: 0 }}
        >
          <CartesianGrid
            vertical={false}
            stroke="var(--grid)"
            strokeWidth={1}
          />
          <XAxis
            dataKey="t"
            type="number"
            scale="time"
            domain={["dataMin", "dataMax"]}
            tickFormatter={(t: number) => fmtClock(t, rangeSeconds)}
            tickLine={false}
            axisLine={{ stroke: "var(--axis)", strokeWidth: 1 }}
            tick={{ fill: "var(--faint-foreground)", fontSize: 10 }}
            tickMargin={6}
            minTickGap={48}
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
          />
          <Tooltip
            cursor={{ stroke: "var(--axis)", strokeWidth: 1 }}
            isAnimationActive={false}
            content={<ChartTooltip series={series} band={band} format={format} />}
          />
          {band ? (
            <Area
              dataKey={(row: Record<string, number | null>) => [
                row[band.lowKey] ?? null,
                row[band.highKey] ?? null,
              ]}
              name="__band"
              type="monotone"
              stroke="none"
              fill={band.color}
              fillOpacity={0.18}
              dot={false}
              activeDot={false}
              connectNulls={false}
              isAnimationActive={false}
            />
          ) : null}
          {series.map((def) => (
            <Area
              key={def.key}
              dataKey={def.key}
              stackId={stacked ? "stack" : def.key}
              type="monotone"
              stroke={def.color}
              strokeWidth={2}
              strokeLinejoin="round"
              strokeLinecap="round"
              fill={fillOpacity > 0 ? def.color : "none"}
              fillOpacity={fillOpacity}
              dot={false}
              activeDot={{
                r: 4,
                fill: def.color,
                stroke: "var(--card)",
                strokeWidth: 2,
              }}
              connectNulls={false}
              isAnimationActive={false}
            />
          ))}
        </AreaChart>
      </ResponsiveContainer>
    </div>
  )
}
