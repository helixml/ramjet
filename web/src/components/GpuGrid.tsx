import { Area, AreaChart, ResponsiveContainer, Tooltip } from "recharts"
import { TriangleAlert } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { Meter } from "@/components/Meter"
import { fmtBytes, fmtClockFull, fmtNum, fmtPct, fmtWatts } from "@/lib/format"
import type { Sample } from "@/lib/api"

interface GpuRow {
  t: number
  util: number | null
}

interface GpuView {
  index: number
  name: string
  utilPct: number | null
  memUsed: number | null
  memTotal: number | null
  powerWatts: number | null
  tempC: number | null
  smMhz: number | null
  trend: GpuRow[]
}

function collect(points: Sample[], latest: Sample | null): GpuView[] {
  const source = latest?.gpus ?? points[points.length - 1]?.gpus ?? []
  return source.map((gpu) => ({
    index: gpu.index,
    name: gpu.name,
    utilPct: gpu.util_pct,
    memUsed: gpu.mem_used_bytes,
    memTotal: gpu.mem_total_bytes,
    powerWatts: gpu.power_watts,
    tempC: gpu.temp_c,
    smMhz: gpu.sm_mhz,
    trend: points.map((sample) => ({
      t: sample.t,
      util: sample.gpus?.find((candidate) => candidate.index === gpu.index)?.util_pct ?? null,
    })),
  }))
}

function TempBadge({ tempC }: { tempC: number | null }) {
  if (tempC == null) return <span className="text-faint-foreground">—</span>
  // Thermal context from the node06 policy: throttle onset at 85°C, the
  // operational guard aborts at 84°C. Warn well before either.
  const hot = tempC >= 83 ? "critical" : tempC >= 75 ? "warning" : null
  return (
    <span className="flex items-center gap-1 tabular-nums">
      {hot ? (
        <TriangleAlert
          aria-label={hot === "critical" ? "near abort ceiling" : "running hot"}
          className="size-3"
          style={{
            color: hot === "critical" ? "var(--status-critical)" : "var(--status-warning)",
          }}
        />
      ) : null}
      {tempC.toFixed(0)}°C
    </span>
  )
}

function GpuCard({ gpu, rangeLabel }: { gpu: GpuView; rangeLabel: string }) {
  const memPct =
    gpu.memUsed != null && gpu.memTotal != null && gpu.memTotal > 0
      ? (gpu.memUsed / gpu.memTotal) * 100
      : null
  return (
    <Card>
      <CardContent className="flex flex-col gap-2 px-3.5 py-3">
        <div className="flex items-baseline justify-between gap-2">
          <span className="text-xs font-medium">GPU {gpu.index}</span>
          <span className="text-lg font-semibold tabular-nums">
            {fmtPct(gpu.utilPct, 0)}
          </span>
        </div>
        <div className="h-12 w-full">
          <ResponsiveContainer width="100%" height="100%">
            <AreaChart
              data={gpu.trend}
              margin={{ top: 2, right: 0, bottom: 0, left: 0 }}
            >
              <Tooltip
                cursor={{ stroke: "var(--axis)", strokeWidth: 1 }}
                isAnimationActive={false}
                content={({ active, payload, label }) => {
                  const value = payload?.[0]?.value
                  if (!active || typeof value !== "number" || typeof label !== "number")
                    return null
                  return (
                    <div className="rounded-md border border-border bg-card px-2 py-1 text-[11px] shadow-md">
                      <span className="font-medium tabular-nums">
                        {fmtPct(value, 0)}
                      </span>
                      <span className="text-faint-foreground ml-1.5 tabular-nums">
                        {fmtClockFull(label)}
                      </span>
                    </div>
                  )
                }}
              />
              <Area
                dataKey="util"
                type="monotone"
                stroke="var(--chart-1)"
                strokeWidth={1.5}
                fill="var(--chart-1)"
                fillOpacity={0.1}
                dot={false}
                activeDot={{
                  r: 4,
                  fill: "var(--chart-1)",
                  stroke: "var(--card)",
                  strokeWidth: 2,
                }}
                isAnimationActive={false}
                connectNulls={false}
              />
            </AreaChart>
          </ResponsiveContainer>
        </div>
        <div className="text-faint-foreground -mt-1 text-[10px]">
          utilization · {rangeLabel}
        </div>
        <Meter
          label="memory"
          pct={memPct}
          tone="neutral"
          detail={`${fmtBytes(gpu.memUsed)} / ${fmtBytes(gpu.memTotal)}`}
        />
        <div className="text-muted-foreground flex items-center justify-between text-[11px]">
          <TempBadge tempC={gpu.tempC} />
          <span className="tabular-nums">{fmtWatts(gpu.powerWatts)}</span>
          <span className="tabular-nums">{fmtNum(gpu.smMhz)} MHz</span>
        </div>
      </CardContent>
    </Card>
  )
}

export function GpuGrid({
  points,
  latest,
  rangeLabel,
}: {
  points: Sample[]
  latest: Sample | null
  rangeLabel: string
}) {
  const gpus = collect(points, latest)
  if (gpus.length === 0) {
    return (
      <Card>
        <CardContent className="text-faint-foreground py-8 text-center text-xs">
          no GPU telemetry — point MD_MACHINEVIEW_AGENT_URL at a running
          bench/machineview_agent.py
        </CardContent>
      </Card>
    )
  }
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-3 lg:grid-cols-4">
      {gpus.map((gpu) => (
        <GpuCard key={gpu.index} gpu={gpu} rangeLabel={rangeLabel} />
      ))}
    </div>
  )
}
