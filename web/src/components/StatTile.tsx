import { useState, type MouseEvent } from "react"
import { Area, AreaChart, ResponsiveContainer } from "recharts"
import { Card, CardContent } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { fmtClockFull } from "@/lib/format"
import type { SparkPoint } from "@/lib/sparkline"

export interface StatTileProps {
  label: string
  value: string
  /** Optional context line under the value (e.g. "of 1.0 TiB"). */
  detail?: string
  /** Trailing trend, one point per even time bucket of the selected range. */
  trend?: SparkPoint[]
  format?: (value: number) => string
  loading?: boolean
}

export function StatTile({
  label,
  value,
  detail,
  trend,
  format,
  loading = false,
}: StatTileProps) {
  const [hoverIndex, setHoverIndex] = useState<number | null>(null)
  const points = trend ?? []
  const drawn = points.filter((point) => typeof point.v === "number" && Number.isFinite(point.v))
  const hover = hoverIndex != null ? points[hoverIndex] : undefined
  const hovered =
    hover != null && typeof hover.v === "number" && Number.isFinite(hover.v) ? hover : null

  function onMove(event: MouseEvent<HTMLDivElement>) {
    if (points.length === 0) return
    const rect = event.currentTarget.getBoundingClientRect()
    const ratio = Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width))
    setHoverIndex(Math.min(points.length - 1, Math.floor(ratio * points.length)))
  }

  return (
    <Card aria-busy={loading || undefined}>
      <CardContent
        className={`relative overflow-hidden px-3.5 py-3 ${drawn.length >= 2 ? "cursor-crosshair" : ""}`}
        onMouseMove={drawn.length >= 2 ? onMove : undefined}
        onMouseLeave={drawn.length >= 2 ? () => setHoverIndex(null) : undefined}
      >
        <div className={drawn.length >= 2 ? "pr-[4.25rem]" : undefined}>
          <div className="text-muted-foreground truncate text-[11px]">{label}</div>
          {loading ? (
            <>
              <Skeleton className="mt-1.5 h-6 w-16" />
              <Skeleton className="mt-1.5 h-2.5 w-10" />
            </>
          ) : (
            <>
              <div className="mt-0.5 font-mono text-xl font-medium leading-tight whitespace-nowrap tabular-nums">
                {hovered && format ? format(hovered.v as number) : value}
              </div>
              {hovered ? (
                <div className="text-faint-foreground truncate text-[10px] tabular-nums">
                  {fmtClockFull(hovered.t)}
                </div>
              ) : detail ? (
                <div className="text-faint-foreground truncate text-[10px]">{detail}</div>
              ) : null}
            </>
          )}
        </div>
        {loading ? (
          <Skeleton className="absolute right-3.5 bottom-3 h-8 w-16" />
        ) : drawn.length >= 2 ? (
          <div className="pointer-events-none absolute right-2.5 bottom-2.5 h-8 w-16" aria-hidden>
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={points} margin={{ top: 2, right: 0, bottom: 0, left: 0 }}>
                <Area
                  dataKey="v"
                  type="monotone"
                  stroke="var(--chart-1)"
                  strokeWidth={1.5}
                  fill="var(--chart-1)"
                  fillOpacity={0.4}
                  dot={false}
                  connectNulls
                  isAnimationActive={false}
                />
              </AreaChart>
            </ResponsiveContainer>
            {hoverIndex != null ? (
              <div
                className="absolute top-0 bottom-0 w-px bg-foreground/50"
                style={{ left: `${((hoverIndex + 0.5) / points.length) * 100}%` }}
              />
            ) : null}
          </div>
        ) : null}
      </CardContent>
    </Card>
  )
}
