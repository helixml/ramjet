import { useMemo, useState } from "react"
import { useDashboardData } from "@/hooks/useDashboardData"
import { TopBar } from "@/components/TopBar"
import { RangePicker, RANGE_OPTIONS } from "@/components/RangePicker"
import { StatTile } from "@/components/StatTile"
import { ChartCard } from "@/components/ChartCard"
import { GpuGrid } from "@/components/GpuGrid"
import { Meter } from "@/components/Meter"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import {
  endpointLabel,
  fmtBps,
  fmtBytes,
  fmtMs,
  fmtNum,
  fmtPct,
  fmtWattHours,
  fmtWatts,
} from "@/lib/format"
import type { Sample } from "@/lib/api"

type Row = { t: number } & Record<string, number | null>

/** Flattens one stored sample into chart-ready keys. */
function toRow(sample: Sample): Row {
  const row: Row = { t: sample.t }
  const host = sample.host
  row.cpu = host?.cpu_pct ?? null
  row.mem_used = host?.mem_used_bytes ?? null
  row.mem_cached = host?.mem_cached_bytes ?? null
  row.net_rx = host?.net_rx_bps ?? null
  row.net_tx = host?.net_tx_bps ?? null
  row.disk_r = host?.disk_read_bps ?? null
  row.disk_w = host?.disk_write_bps ?? null
  const serving = sample.serving
  row.gen_tps = serving?.gen_tps ?? null
  row.prompt_tps = serving?.prompt_tps ?? null
  row.cached_tps = serving?.cached_tps ?? null
  row.ttft_p50 = serving?.ttft_p50_ms ?? null
  row.ttft_p95 = serving?.ttft_p95_ms ?? null
  row.inflight = serving?.inflight ?? null
  row.hit_lb = serving?.cache_hit_pct ?? null
  const engines = sample.engines ?? []
  engines.forEach((engine, index) => {
    row[`run_${index}`] = engine.running
    row[`wait_${index}`] = engine.waiting
    row[`kv_${index}`] = engine.kv_cache_pct
    row[`hit_${index}`] = engine.prefix_hit_pct
  })
  const upstreams = sample.serving?.upstreams ?? []
  upstreams.forEach((upstream, index) => {
    row[`rps_${index}`] = upstream.requests_per_second
  })
  row.watts_gpu = sample.energy?.gpu_watts ?? null
  row.watts_cpu = sample.energy?.cpu_watts ?? null
  row.wh = sample.energy?.total_watt_hours ?? null
  return row
}

function trend(rows: Row[], key: string): Array<number | null> {
  const stride = Math.max(1, Math.floor(rows.length / 12))
  return rows.filter((_, index) => index % stride === 0).map((row) => row[key] ?? null)
}

function SectionTitle({ children }: { children: string }) {
  return (
    <h2 className="text-muted-foreground mt-2 text-xs font-medium uppercase tracking-wider">
      {children}
    </h2>
  )
}

export default function App() {
  const [rangeSeconds, setRangeSeconds] = useState(3600)
  const { summary, series, error, mock } = useDashboardData(rangeSeconds)
  const points = useMemo(() => series?.points ?? [], [series])
  const rows = useMemo(() => points.map(toRow), [points])
  const latest = summary?.latest ?? null
  const rangeLabel =
    RANGE_OPTIONS.find((option) => option.seconds === rangeSeconds)?.label ?? ""

  // Engine identity is positional and stable: slot 2 for A, 3 for B, …
  // (slot 1 stays the load balancer / aggregate color).
  const engineDefs = (summary?.upstreams ?? []).slice(0, 6).map((endpoint, index) => ({
    index,
    label: endpointLabel(endpoint, index),
    color: `var(--chart-${index + 2})`,
  }))

  const latestServing = latest?.serving
  const latestHost = latest?.host
  const latestEngines = latest?.engines ?? []
  const kvAvg = latestEngines.length
    ? latestEngines.reduce((sum, engine) => sum + (engine.kv_cache_pct ?? 0), 0) /
      latestEngines.length
    : null
  const powerNow =
    latest?.energy != null
      ? (latest.energy.gpu_watts ?? 0) + (latest.energy.cpu_watts ?? 0)
      : null
  const memPct =
    latestHost?.mem_used_bytes != null &&
    latestHost?.mem_total_bytes != null &&
    latestHost.mem_total_bytes > 0
      ? (latestHost.mem_used_bytes / latestHost.mem_total_bytes) * 100
      : null
  const swapPct =
    latestHost?.swap_used_bytes != null &&
    latestHost?.swap_total_bytes != null &&
    latestHost.swap_total_bytes > 0
      ? (latestHost.swap_used_bytes / latestHost.swap_total_bytes) * 100
      : null

  return (
    <div className="mx-auto flex max-w-[1400px] flex-col gap-4 px-4 py-4 md:px-6">
      <TopBar
        hostname={summary?.hostname ?? null}
        live={error == null && summary != null}
        mock={mock}
      />

      {/* The one filter row — everything below renders against this slice. */}
      <div className="flex flex-wrap items-center justify-between gap-2">
        <RangePicker value={rangeSeconds} onChange={setRangeSeconds} />
        {error ? (
          <span className="text-xs" style={{ color: "var(--status-critical)" }}>
            ⚠ {error}
          </span>
        ) : (
          <span className="text-faint-foreground text-[11px]">
            refreshes every 5 s · stored locally by the load balancer
          </span>
        )}
      </div>

      <div className="grid grid-cols-2 gap-3 md:grid-cols-4 xl:grid-cols-8">
        <StatTile
          label="Gen tok/s"
          value={fmtNum(latestServing?.gen_tps)}
          trend={trend(rows, "gen_tps")}
        />
        <StatTile
          label="TTFT p95"
          value={fmtMs(latestServing?.ttft_p95_ms)}
          trend={trend(rows, "ttft_p95")}
        />
        <StatTile
          label="In flight"
          value={fmtNum(latestServing?.inflight)}
          trend={trend(rows, "inflight")}
        />
        <StatTile
          label="KV cache"
          value={fmtPct(kvAvg)}
          detail={latestEngines.length ? `${latestEngines.length} engines` : undefined}
        />
        <StatTile
          label="Cache hit"
          value={fmtPct(latestServing?.cache_hit_pct)}
          trend={trend(rows, "hit_lb")}
        />
        <StatTile
          label="CPU"
          value={fmtPct(latestHost?.cpu_pct)}
          detail={
            latestHost?.load1 != null ? `load ${fmtNum(latestHost.load1, 1)}` : undefined
          }
          trend={trend(rows, "cpu")}
        />
        <StatTile
          label="Memory"
          value={fmtBytes(latestHost?.mem_used_bytes)}
          detail={
            latestHost?.mem_total_bytes != null
              ? `of ${fmtBytes(latestHost.mem_total_bytes)}`
              : undefined
          }
        />
        <StatTile
          label="Power"
          value={fmtWatts(powerNow)}
          detail={
            latest?.energy != null
              ? fmtWattHours(latest.energy.total_watt_hours)
              : undefined
          }
          trend={trend(rows, "watts_gpu")}
        />
      </div>

      <SectionTitle>Serving</SectionTitle>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        <ChartCard
          title="Token throughput"
          description="tokens per second through the load balancer"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtNum(v)}
          series={[
            { key: "gen_tps", label: "generated", color: "var(--chart-1)" },
            { key: "prompt_tps", label: "prompt", color: "var(--chart-2)" },
            { key: "cached_tps", label: "prompt cached", color: "var(--chart-3)" },
          ]}
        />
        <ChartCard
          title="Time to first token"
          description="window quantiles over the request stream"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtMs(v)}
          series={[
            { key: "ttft_p95", label: "p95", color: "var(--chart-1)" },
            { key: "ttft_p50", label: "p50", color: "var(--chart-1-soft)" },
          ]}
        />
        <ChartCard
          title="Requests in flight"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtNum(v)}
          series={[{ key: "inflight", label: "in flight", color: "var(--chart-1)" }]}
        />
        <ChartCard
          title="Running per engine"
          description="requests in the running batch"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtNum(v)}
          stacked
          series={engineDefs.map((engine) => ({
            key: `run_${engine.index}`,
            label: engine.label,
            color: engine.color,
          }))}
        />
        <ChartCard
          title="KV cache usage"
          description="per-engine GPU KV cache occupancy"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtPct(v)}
          domain={[0, 100]}
          series={engineDefs.map((engine) => ({
            key: `kv_${engine.index}`,
            label: engine.label,
            color: engine.color,
          }))}
        />
        <ChartCard
          title="Prefix cache hit rate"
          description="LB token-weighted vs engine-reported"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtPct(v)}
          domain={[0, 100]}
          series={[
            { key: "hit_lb", label: "LB tokens", color: "var(--chart-1)" },
            ...engineDefs.map((engine) => ({
              key: `hit_${engine.index}`,
              label: engine.label,
              color: engine.color,
            })),
          ]}
        />
        <ChartCard
          title="Request rate per upstream"
          description="requests per second after routing"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtNum(v, 1)}
          series={engineDefs.map((engine) => ({
            key: `rps_${engine.index}`,
            label: engine.label,
            color: engine.color,
          }))}
        />
        <ChartCard
          title="Waiting per engine"
          description="queued requests not yet scheduled"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtNum(v)}
          stacked
          series={engineDefs.map((engine) => ({
            key: `wait_${engine.index}`,
            label: engine.label,
            color: engine.color,
          }))}
        />
      </div>

      <SectionTitle>Machine</SectionTitle>
      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        <ChartCard
          title="CPU"
          description="host busy share, all cores"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtPct(v)}
          domain={[0, 100]}
          series={[{ key: "cpu", label: "busy", color: "var(--chart-1)" }]}
        />
        <ChartCard
          title="Memory"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtBytes(v)}
          stacked
          series={[
            { key: "mem_used", label: "used", color: "var(--chart-1)" },
            { key: "mem_cached", label: "cache/buffers", color: "var(--chart-1-soft)" },
          ]}
        />
        <ChartCard
          title="Network"
          description="host interfaces, virtual devices excluded"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtBps(v)}
          series={[
            { key: "net_rx", label: "receive", color: "var(--chart-1)" },
            { key: "net_tx", label: "transmit", color: "var(--chart-2)" },
          ]}
        />
        <ChartCard
          title="Disk I/O"
          description="whole-disk transfer rates"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtBps(v)}
          series={[
            { key: "disk_r", label: "read", color: "var(--chart-1)" },
            { key: "disk_w", label: "write", color: "var(--chart-2)" },
          ]}
        />
        <ChartCard
          title="Power draw"
          description="GPU board power plus CPU package (RAPL)"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtWatts(v)}
          stacked
          series={[
            { key: "watts_gpu", label: "GPUs", color: "var(--chart-1)" },
            { key: "watts_cpu", label: "CPU", color: "var(--chart-2)" },
          ]}
        />
        <Card>
          <CardHeader>
            <CardTitle>Storage</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-col gap-3">
            {(latestHost?.disks ?? []).map((disk) => (
              <Meter
                key={disk.mount}
                label={disk.mount}
                pct={disk.total_bytes > 0 ? (disk.used_bytes / disk.total_bytes) * 100 : null}
                detail={`${fmtBytes(disk.used_bytes)} / ${fmtBytes(disk.total_bytes)}`}
              />
            ))}
            {memPct != null ? <Meter label="memory" pct={memPct} /> : null}
            {swapPct != null ? <Meter label="swap" pct={swapPct} /> : null}
            {(latestHost?.disks ?? []).length === 0 && memPct == null ? (
              <div className="text-faint-foreground py-6 text-center text-xs">
                no host telemetry — run bench/machineview_agent.py
              </div>
            ) : null}
          </CardContent>
        </Card>
        <ChartCard
          title="Energy"
          description="cumulative since the load balancer started"
          data={rows}
          rangeSeconds={rangeSeconds}
          format={(v) => fmtWattHours(v)}
          series={[{ key: "wh", label: "energy", color: "var(--chart-1)" }]}
        />
      </div>

      <SectionTitle>GPUs</SectionTitle>
      <GpuGrid points={points} latest={latest} rangeLabel={rangeLabel} />

      <footer className="text-faint-foreground pb-4 pt-2 text-[11px]">
        Metrics are sampled and stored locally by the mini-dynamo load balancer —
        no Prometheus or Grafana required.
      </footer>
    </div>
  )
}
