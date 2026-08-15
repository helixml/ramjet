import { Area, AreaChart, ResponsiveContainer } from "recharts"
import { Card, CardContent } from "@/components/ui/card"

export interface StatTileProps {
  label: string
  value: string
  /** Optional context line under the value (e.g. "of 1.0 TiB"). */
  detail?: string
  /** Trailing trend, plotted as a quiet 12-point sparkline. */
  trend?: Array<number | null>
}

export function StatTile({ label, value, detail, trend }: StatTileProps) {
  const points = (trend ?? [])
    .filter((v): v is number => typeof v === "number" && Number.isFinite(v))
    .slice(-12)
    .map((v, i) => ({ i, v }))
  return (
    <Card>
      <CardContent className="flex items-end justify-between gap-2 px-3.5 py-3">
        <div className="min-w-0">
          <div className="text-muted-foreground truncate text-[11px]">{label}</div>
          <div className="mt-0.5 whitespace-nowrap text-xl font-semibold leading-tight">{value}</div>
          {detail ? (
            <div className="text-faint-foreground truncate text-[10px]">{detail}</div>
          ) : null}
        </div>
        {points.length >= 3 ? (
          <div className="h-8 w-16 shrink-0" aria-hidden>
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={points} margin={{ top: 2, right: 2, bottom: 0, left: 2 }}>
                <Area
                  dataKey="v"
                  type="monotone"
                  stroke="var(--chart-1)"
                  strokeWidth={1.5}
                  fill="var(--chart-1)"
                  fillOpacity={0.08}
                  dot={false}
                  isAnimationActive={false}
                />
              </AreaChart>
            </ResponsiveContainer>
          </div>
        ) : null}
      </CardContent>
    </Card>
  )
}
