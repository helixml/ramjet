import { TriangleAlert, Zap } from "lucide-react"
import { Card, CardContent } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { Meter } from "@/components/Meter"
import { TimeChart } from "@/components/TimeChart"
import { fmtBytes, fmtNum, fmtPct, fmtWatts } from "@/lib/format"
import type { GpuSample, Sample } from "@/lib/api"

interface GpuView {
  latest: GpuSample
  trend: Array<{ t: number } & Record<string, number | null>>
}

function collect(points: Sample[], latest: Sample | null): GpuView[] {
  const source = latest?.gpus ?? points[points.length - 1]?.gpus ?? []
  return source.map((gpu) => ({
    latest: gpu,
    trend: points.map((sample) => {
      const at = sample.gpus?.find((candidate) => candidate.index === gpu.index)
      return {
        t: sample.t,
        util: at?.util_pct ?? null,
        mem_util: at?.mem_util_pct ?? null,
      }
    }),
  }))
}

/**
 * Throttle badges: reserved status colors with icon + label, never color
 * alone. A software power cap is routine under sustained load; hardware
 * slowdowns mean the silicon is protecting itself.
 */
function ThrottleBadges({ gpu }: { gpu: GpuSample }) {
  const reasons: Array<{ label: string; color: string; hardware: boolean }> = []
  if ((gpu.throttle_sw_power ?? 0) > 0.5) {
    reasons.push({ label: "power cap", color: "var(--status-warning)", hardware: false })
  }
  if ((gpu.throttle_sw_thermal ?? 0) > 0.5) {
    reasons.push({ label: "thermal (sw)", color: "var(--status-serious)", hardware: false })
  }
  if ((gpu.throttle_hw_thermal ?? 0) > 0.5) {
    reasons.push({ label: "thermal (hw)", color: "var(--status-critical)", hardware: true })
  }
  if ((gpu.throttle_hw ?? 0) > 0.5) {
    reasons.push({ label: "hw slowdown", color: "var(--status-critical)", hardware: true })
  }
  if (reasons.length === 0) {
    return <span className="text-faint-foreground text-[11px]">no throttling</span>
  }
  return (
    <span className="flex flex-wrap items-center gap-2">
      {reasons.map((reason) => (
        <span
          key={reason.label}
          className="flex items-center gap-1 text-[11px] font-medium"
        >
          {reason.hardware ? (
            <TriangleAlert aria-hidden className="size-3" style={{ color: reason.color }} />
          ) : (
            <Zap aria-hidden className="size-3" style={{ color: reason.color }} />
          )}
          {reason.label}
        </span>
      ))}
    </span>
  )
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col">
      <span className="text-faint-foreground text-[10px]">{label}</span>
      <span className="text-xs font-medium tabular-nums">{value}</span>
    </div>
  )
}

function TempStat({ label, tempC }: { label: string; tempC: number | null | undefined }) {
  if (tempC == null) return <Stat label={label} value="—" />
  // Throttle onset is 85°C on these parts; the node06 guard aborts earlier.
  const hot = tempC >= 83 ? "var(--status-critical)" : tempC >= 75 ? "var(--status-warning)" : null
  return (
    <div className="flex flex-col">
      <span className="text-faint-foreground text-[10px]">{label}</span>
      <span className="flex items-center gap-1 text-xs font-medium tabular-nums">
        {hot ? <TriangleAlert aria-label="running hot" className="size-3" style={{ color: hot }} /> : null}
        {tempC.toFixed(0)}°C
      </span>
    </div>
  )
}

function GpuRow({ gpu, rangeSeconds }: { gpu: GpuView; rangeSeconds: number }) {
  const g = gpu.latest
  const memPct =
    g.mem_used_bytes != null && g.mem_total_bytes != null && g.mem_total_bytes > 0
      ? (g.mem_used_bytes / g.mem_total_bytes) * 100
      : null
  const power =
    g.power_limit_watts != null && g.power_limit_watts > 0
      ? `${fmtNum(g.power_watts)} / ${fmtNum(g.power_limit_watts)} W`
      : fmtWatts(g.power_watts)
  const hasMemUtil = gpu.trend.some((row) => typeof row.mem_util === "number")
  return (
    <Card>
      <CardContent className="flex flex-col gap-3 px-4 py-3 lg:flex-row lg:items-stretch">
        <div className="flex w-full shrink-0 flex-col gap-2.5 lg:w-60">
          <div className="flex items-baseline justify-between gap-2">
            <span className="text-sm font-semibold">GPU {g.index}</span>
            <span className="text-xl font-semibold tabular-nums">
              {fmtPct(g.util_pct, 0)}
            </span>
          </div>
          <Meter
            label="memory"
            pct={memPct}
            tone="neutral"
            detail={`${fmtBytes(g.mem_used_bytes)} / ${fmtBytes(g.mem_total_bytes)}`}
          />
          <div className="grid grid-cols-3 gap-x-3 gap-y-2">
            <TempStat label="core" tempC={g.temp_c} />
            <TempStat label="memory" tempC={g.temp_mem_c} />
            <Stat label="power" value={power} />
            <Stat label="SM clock" value={`${fmtNum(g.sm_mhz)} MHz`} />
            <Stat label="mem clock" value={`${fmtNum(g.mem_clock_mhz)} MHz`} />
            <Stat
              label="fan · pstate"
              value={`${g.fan_pct != null ? fmtPct(g.fan_pct, 0) : "—"} · ${
                g.pstate != null ? `P${g.pstate.toFixed(0)}` : "—"
              }`}
            />
          </div>
          <ThrottleBadges gpu={g} />
        </div>
        <div className="min-w-0 flex-1">
          <div className="text-muted-foreground mb-1 flex items-center gap-3 text-[11px]">
            <span className="flex items-center gap-1.5">
              <span
                aria-hidden
                className="h-0.5 w-3 rounded-full"
                style={{ background: "var(--chart-1)" }}
              />
              SM busy
            </span>
            {hasMemUtil ? (
              <span className="flex items-center gap-1.5">
                <span
                  aria-hidden
                  className="h-0.5 w-3 rounded-full"
                  style={{ background: "var(--chart-1-soft)" }}
                />
                memory controller
              </span>
            ) : null}
          </div>
          <TimeChart
            data={gpu.trend}
            rangeSeconds={rangeSeconds}
            format={(v) => fmtPct(v)}
            domain={[0, 100]}
            height={132}
            series={[
              { key: "util", label: "SM busy", color: "var(--chart-1)" },
              ...(hasMemUtil
                ? [
                    {
                      key: "mem_util",
                      label: "memory controller",
                      color: "var(--chart-1-soft)",
                    },
                  ]
                : []),
            ]}
          />
        </div>
      </CardContent>
    </Card>
  )
}

export function GpuGrid({
  points,
  latest,
  rangeSeconds,
  loading = false,
}: {
  points: Sample[]
  latest: Sample | null
  rangeSeconds: number
  loading?: boolean
}) {
  if (loading) {
    return (
      <div className="flex flex-col gap-3" aria-busy>
        {Array.from({ length: 4 }, (_, index) => (
          <Card key={index}>
            <CardContent className="flex flex-col gap-3 px-4 py-3 lg:flex-row lg:items-stretch">
              <div className="flex w-full shrink-0 flex-col gap-2.5 lg:w-60">
                <div className="flex items-baseline justify-between gap-2">
                  <Skeleton className="h-4 w-16" />
                  <Skeleton className="h-7 w-12" />
                </div>
                <Skeleton className="h-1.5 w-full rounded-full" />
                <div className="grid grid-cols-3 gap-x-3 gap-y-2">
                  {Array.from({ length: 6 }, (_, cell) => (
                    <div key={cell} className="flex flex-col gap-1">
                      <Skeleton className="h-2.5 w-10" />
                      <Skeleton className="h-3.5 w-14" />
                    </div>
                  ))}
                </div>
              </div>
              <Skeleton className="min-h-[132px] min-w-0 flex-1" />
            </CardContent>
          </Card>
        ))}
      </div>
    )
  }
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
    <div className="flex flex-col gap-3">
      {gpus.map((gpu) => (
        <GpuRow key={gpu.latest.index} gpu={gpu} rangeSeconds={rangeSeconds} />
      ))}
    </div>
  )
}
