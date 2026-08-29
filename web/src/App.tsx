import { useMemo, useState } from "react"
import { useDashboardData } from "@/hooks/useDashboardData"
import { useTokenHistory } from "@/hooks/useTokenHistory"
import { TopBar } from "@/components/TopBar"
import { RangePicker } from "@/components/RangePicker"
import { StatTile } from "@/components/StatTile"
import { ChartCard, type ChartCardProps } from "@/components/ChartCard"
import { HeatmapCard, buildScale } from "@/components/Heatmap"
import { dayGrid, hourGrid, totals } from "@/lib/tokens"
import { GpuGrid } from "@/components/GpuGrid"
import { AdaptiveTopology } from "@/components/AdaptiveTopology"
import { Meter } from "@/components/Meter"
import { TabBar, useTabs, type TabDef } from "@/components/Tabs"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import {
  endpointLabel,
  fmtBps,
  fmtBytes,
  fmtCount,
  fmtMs,
  fmtNum,
  fmtPct,
  fmtWattHours,
  fmtWatts,
} from "@/lib/format"
import { useLiveStream } from "@/hooks/useLiveStream"
import { sparkline, windowMean, TILE_WINDOW_MS } from "@/lib/sparkline"
import { rollingAverage, windowLabel } from "@/lib/rolling"
import type { Sample, ServingSample } from "@/lib/api"

type Row = { t: number } & Record<string, number | null>

/**
 * The load-balancer-derived half of a row. These are the keys the live
 * stream can fill on its own, because they come from the proxy's own
 * registry rather than from scraping engines or the host agent.
 */
function toServingRow(t: number, serving: ServingSample | undefined): Row {
  const row: Row = { t }
  row.gen_tps = serving?.gen_tps ?? null
  row.prompt_tps = serving?.prompt_tps ?? null
  row.cached_tps = serving?.cached_tps ?? null
  row.ttft_p50 = serving?.ttft_p50_ms ?? null
  row.ttft_p95 = serving?.ttft_p95_ms ?? null
  row.tpot_p95 = serving?.tpot_p95_ms ?? null
  row.stream_tps_p50 = serving?.stream_tps_p50 ?? null
  row.stream_tps_p05 = serving?.stream_tps_p05 ?? null
  row.inflight = serving?.inflight ?? null
  row.hit_lb = serving?.cache_hit_pct ?? null
  ;(serving?.upstreams ?? []).forEach((upstream, index) => {
    row[`rps_${index}`] = upstream.requests_per_second
  })
  return row
}

/** Flattens one stored sample into chart-ready keys. */
function toRow(sample: Sample): Row {
  const row: Row = toServingRow(sample.t, sample.serving)
  const host = sample.host
  row.cpu = host?.cpu_pct ?? null
  row.mem_used = host?.mem_used_bytes ?? null
  row.mem_cached = host?.mem_cached_bytes ?? null
  row.net_rx = host?.net_rx_bps ?? null
  row.net_tx = host?.net_tx_bps ?? null
  row.disk_r = host?.disk_read_bps ?? null
  row.disk_w = host?.disk_write_bps ?? null
  row.disk_riops = host?.disk_read_iops ?? null
  row.disk_wiops = host?.disk_write_iops ?? null
  row.disk_util = host?.disk_util_pct ?? null
  row.iowait = host?.iowait_pct ?? null
  row.io_pressure = host?.io_pressure_pct ?? null
  const engines = sample.engines ?? []
  engines.forEach((engine, index) => {
    row[`run_${index}`] = engine.running
    row[`wait_${index}`] = engine.waiting
    row[`kv_${index}`] = engine.kv_cache_pct
    row[`hit_${index}`] = engine.prefix_hit_pct
  })
  const gpus = sample.gpus ?? []
  gpus.forEach((gpu) => {
    row[`gpu_${gpu.index}`] = gpu.util_pct ?? null
  })
  const engineHits = engines
    .map((engine) => engine.prefix_hit_pct)
    .filter((v): v is number => typeof v === "number")
  row.hit_engines = engineHits.length
    ? engineHits.reduce((sum, v) => sum + v, 0) / engineHits.length
    : null
  row.watts_gpu = sample.energy?.gpu_watts ?? null
  row.watts_cpu = sample.energy?.cpu_watts ?? null
  row.wh = sample.energy?.total_watt_hours ?? null
  return row
}

const TABS: TabDef[] = [
  { id: "overview", label: "Overview" },
  { id: "topology", label: "Topology" },
  { id: "serving", label: "Serving" },
  { id: "gpus", label: "GPUs" },
  { id: "system", label: "System" },
]

/** Days of hourly history requested; the LB clamps to its own retention. */
const HISTORY_DAYS = 30

/** Row key the rolling cache-hit average is written to. */
const CACHE_HIT_KEY = "hit_avg"

function ChartGrid({ cards, loading }: { cards: ChartCardProps[]; loading?: boolean }) {
  return (
    <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
      {cards.map((card) => (
        <ChartCard key={card.title} {...card} loading={loading} />
      ))}
    </div>
  )
}

export default function App() {
  const [rangeSeconds, setRangeSeconds] = useState(3600)
  const { active: tab, select: selectTab } = useTabs(TABS, "overview")
  const { summary, series, error, mock } = useDashboardData(rangeSeconds)
  const { tokens, error: tokensError } = useTokenHistory(HISTORY_DAYS)
  const live = useLiveStream()
  const loading = summary == null && error == null
  const points = useMemo(() => series?.points ?? [], [series])
  const rows = useMemo(() => points.map(toRow), [points])
  // Live frames replace the polled tail for load-balancer-derived series;
  // engine and host series keep the sampled resolution they are scraped at.
  const servingRows = useMemo(() => {
    if (live.frames.length === 0) return rows
    const liveRows = live.frames.map((frame) => toServingRow(frame.t, frame.serving))
    const from = liveRows[0].t
    return [...rows.filter((row) => row.t < from), ...liveRows]
  }, [rows, live.frames])
  const latest = summary?.latest ?? live.sample ?? null

  // Engine identity is positional and stable: slot 2 for A, 3 for B, …
  // (slot 1 stays the load balancer / aggregate color).
  const engineDefs = (summary?.upstreams ?? []).slice(0, 6).map((endpoint, index) => ({
    index,
    label: endpointLabel(endpoint, index),
    color: `var(--chart-${index + 2})`,
  }))

  // The newest live frame is a second old at most; the polled sample can
  // be five.
  const latestServing = live.frames.at(-1)?.serving ?? latest?.serving
  const servingNow = servingRows.at(-1)?.t ?? Date.now()
  const sparkWindowMs = rangeSeconds * 1000
  // Mean, not max: the counter books a request's whole completion count in
  // the sample where it finished, so a 30s max reads a single big turn's
  // completion tick as thousands of tok/s the fleet never sustained.
  const genTpsAvg = useMemo(
    () => windowMean(servingRows, "gen_tps", servingNow, TILE_WINDOW_MS),
    [servingRows, servingNow],
  )
  // A hit rate measured over a few seconds of bursty traffic is 0% or 100%
  // and rarely anything between, and an idle interval has no ratio at all --
  // plotted raw that is a comb of hairlines, not a rate. Average it over a
  // trailing window sized from the visible range, which is both the quantity
  // people mean by "hit rate" and a line that survives the gaps between
  // bursts. Five percent of the range keeps roughly twenty of the four
  // hundred fetched points under each output point.
  const cacheWindowMs = useMemo(
    () => Math.min(Math.max(rangeSeconds * 1000 * 0.05, 60_000), 600_000),
    [rangeSeconds],
  )
  // `hit_lb` is token-weighted, whichever layer produced it. `hit_engines` is
  // an unweighted mean of per-engine percentages, so a nearly idle engine
  // counts as much as one carrying the fleet -- only reach for it when the
  // weighted figure is absent entirely.
  const cacheHitSource = useMemo(() => {
    for (const frames of [live.frames, points]) {
      for (let index = frames.length - 1; index >= 0; index -= 1) {
        const source = frames[index].serving?.cache_hit_source
        if (source != null) return source
      }
    }
    return undefined
  }, [live.frames, points])
  const cacheHit = useMemo(() => {
    // The 1 Hz stream reads only the proxy's own registry, so a fleet whose
    // hit rate comes from the engines' prefix-cache counters publishes it on
    // the 5 s sampled rows and never on a live frame. Splicing the live tail
    // in would end the line minutes short of now, so that case stays on the
    // polled series.
    const liveHasHit = live.frames.some(
      (frame) => typeof frame.serving?.cache_hit_pct === "number",
    )
    const weighted = (liveHasHit ? servingRows : rows).some(
      (row) => typeof row.hit_lb === "number",
    )
    const source = weighted && liveHasHit ? servingRows : rows
    const rolled = rollingAverage(source, {
      key: weighted ? "hit_lb" : "hit_engines",
      weightKey: "prompt_tps",
      windowMs: cacheWindowMs,
      outKey: CACHE_HIT_KEY,
    })
    const description = weighted
      ? cacheHitSource === "engine_prefix_cache"
        ? "engine-reported prefix cache"
        : "cached share of prompt tokens served"
      : "unweighted engine prefix-cache mean"
    const detail = weighted
      ? cacheHitSource === "engine_prefix_cache"
        ? "engine-reported"
        : "responses"
      : "engine mean"
    const label = windowLabel(cacheWindowMs)
    // The newest point, not the newest non-null one: the average is absent
    // exactly when nothing was served in the trailing window, and holding a
    // stale figure there would read as live.
    const newest = rolled.at(-1)
    return {
      rows: rolled,
      value: newest?.[CACHE_HIT_KEY] ?? null,
      now: newest?.t ?? servingNow,
      description: `${description} · ${label} rolling average`,
      detail: `${detail} · ${label} avg`,
    }
  }, [rows, servingRows, servingNow, live.frames, cacheHitSource, cacheWindowMs])
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

  const shared = { data: rows, rangeSeconds }
  // Cards whose every series comes from the proxy's own registry.
  const liveShared = { data: servingRows, rangeSeconds }
  const engineSeries = (prefix: string) =>
    engineDefs.map((engine) => ({
      key: `${prefix}_${engine.index}`,
      label: engine.label,
      color: engine.color,
    }))

  const servingCards: ChartCardProps[] = [
    {
      ...liveShared,
      title: "Token throughput",
      description: "tokens per second through the load balancer",
      format: (v) => fmtNum(v),
      series: [
        { key: "gen_tps", label: "generated", color: "var(--chart-1)" },
        { key: "prompt_tps", label: "prompt", color: "var(--chart-2)" },
        { key: "cached_tps", label: "prompt cached", color: "var(--chart-3)" },
      ],
    },
    {
      ...liveShared,
      title: "Time to first token",
      description: "window quantiles over the request stream",
      format: (v) => fmtMs(v),
      series: [
        { key: "ttft_p95", label: "p95", color: "var(--chart-1)" },
        { key: "ttft_p50", label: "p50", color: "var(--chart-1-soft)" },
      ],
    },
    {
      ...liveShared,
      title: "Time per output token",
      description: "p95 decode interval after the first token",
      format: (v) => fmtMs(v),
      series: [{ key: "tpot_p95", label: "p95", color: "var(--chart-1)" }],
    },
    {
      ...liveShared,
      title: "Requests in flight",
      format: (v) => fmtNum(v),
      series: [{ key: "inflight", label: "in flight", color: "var(--chart-1)" }],
    },
    {
      ...shared,
      title: "Running per engine",
      description: "requests in the running batch",
      format: (v) => fmtNum(v),
      stacked: true,
      series: engineSeries("run"),
    },
    {
      ...shared,
      title: "Waiting per engine",
      description: "queued requests not yet scheduled",
      format: (v) => fmtNum(v),
      stacked: true,
      series: engineSeries("wait"),
    },
    {
      ...shared,
      title: "KV cache usage",
      description: "per-engine GPU KV cache occupancy",
      format: (v) => fmtPct(v),
      domain: [0, 100],
      series: engineSeries("kv"),
    },
    {
      ...shared,
      title: "Prefix cache hit rate",
      description: "LB token-weighted vs engine-reported",
      format: (v) => fmtPct(v),
      domain: [0, 100],
      series: [
        { key: "hit_lb", label: "LB tokens", color: "var(--chart-1)" },
        ...engineSeries("hit"),
      ],
    },
    {
      ...liveShared,
      title: "Request rate per upstream",
      description: "requests per second after routing",
      format: (v) => fmtNum(v, 1),
      series: engineSeries("rps"),
    },
  ]

  const computeCards: ChartCardProps[] = [
    {
      ...shared,
      title: "CPU",
      description: "host busy share, all cores",
      format: (v) => fmtPct(v),
      domain: [0, 100],
      series: [{ key: "cpu", label: "busy", color: "var(--chart-1)" }],
    },
    {
      ...shared,
      title: "Memory",
      format: (v) => fmtBytes(v),
      stacked: true,
      series: [
        { key: "mem_used", label: "used", color: "var(--chart-1)" },
        { key: "mem_cached", label: "cache/buffers", color: "var(--chart-1-soft)" },
      ],
    },
    {
      ...shared,
      title: "Network",
      description: "host interfaces, virtual devices excluded",
      format: (v) => fmtBps(v),
      series: [
        { key: "net_rx", label: "receive", color: "var(--chart-1)" },
        { key: "net_tx", label: "transmit", color: "var(--chart-2)" },
      ],
    },
  ]

  const diskCards: ChartCardProps[] = [
    {
      ...shared,
      title: "Disk I/O",
      description: "whole-disk transfer rates",
      format: (v) => fmtBps(v),
      series: [
        { key: "disk_r", label: "read", color: "var(--chart-1)" },
        { key: "disk_w", label: "write", color: "var(--chart-2)" },
      ],
    },
    {
      ...shared,
      title: "Disk pressure",
      description:
        latestHost?.disk_inflight != null
          ? `queue depth ${fmtCount(latestHost.disk_inflight, 1)}`
          : "iowait, IO stall, busiest disk",
      format: (v) => fmtPct(v),
      domain: [0, 100],
      series: [
        { key: "iowait", label: "iowait", color: "var(--chart-1)" },
        { key: "io_pressure", label: "IO stall", color: "var(--chart-2)" },
        { key: "disk_util", label: "disk busy", color: "var(--chart-3)" },
      ],
    },
    {
      ...shared,
      title: "Disk IOPS",
      description: "completed read and write operations",
      format: (v) => `${fmtCount(v, 1)}/s`,
      series: [
        { key: "disk_riops", label: "read", color: "var(--chart-1)" },
        { key: "disk_wiops", label: "write", color: "var(--chart-2)" },
      ],
    },
  ]

  const powerCards: ChartCardProps[] = [
    {
      ...shared,
      title: "Power draw",
      description: "GPU board power plus CPU package (RAPL)",
      format: (v) => fmtWatts(v),
      stacked: true,
      series: [
        { key: "watts_gpu", label: "GPUs", color: "var(--chart-1)" },
        { key: "watts_cpu", label: "CPU", color: "var(--chart-2)" },
      ],
    },
    {
      ...shared,
      title: "Energy",
      description: "cumulative since the load balancer started",
      format: (v) => fmtWattHours(v),
      series: [{ key: "wh", label: "energy", color: "var(--chart-1)" }],
    },
  ]

  const cacheHitCard: ChartCardProps = {
    data: cacheHit.rows,
    rangeSeconds,
    title: "Cache hit rate",
    description: cacheHit.description,
    format: (v) => fmtPct(v),
    domain: [0, 100],
    series: [{ key: CACHE_HIT_KEY, label: "hit rate", color: "var(--chart-1)" }],
  }

  const gpuSource = latest?.gpus ?? points[points.length - 1]?.gpus ?? []
  const gpuDefs = gpuSource.map((gpu) => ({
    index: gpu.index,
    label: `GPU ${gpu.index}`,
    color: `hsl(${(gpu.index * 360) / Math.max(gpuSource.length, 1)}, var(--chart-saturation), var(--chart-lightness))`,
  }))
  const gpuCount = gpuDefs.length
  const gpuUtilCard: ChartCardProps = {
    ...shared,
    title: "GPU utilization",
    description: gpuCount > 1 ? "SM busy share per device" : "SM busy share",
    format: (v) => fmtPct(v),
    domain: [0, 100],
    height: 200,
    series: gpuDefs.map((gpu) => ({
      key: `gpu_${gpu.index}`,
      label: gpu.label,
      color: gpu.color,
    })),
  }

  // Token history is its own, much longer series: hourly buckets kept for a
  // month, independent of the range picker that drives every other card.
  const tokenBuckets = useMemo(() => tokens?.buckets ?? [], [tokens])
  const tokenTotals = useMemo(() => totals(tokenBuckets), [tokenBuckets])
  const historyWindow = tokens?.days ?? HISTORY_DAYS
  const tokenDays = useMemo(
    () => dayGrid(tokenBuckets, tokens?.now ?? Date.now(), historyWindow),
    [tokenBuckets, tokens?.now, historyWindow],
  )
  const tokenHours = useMemo(() => hourGrid(tokenBuckets), [tokenBuckets])
  const dayScale = useMemo(
    () => buildScale(tokenDays.cells.map((cell) => cell.value)),
    [tokenDays],
  )
  const hourScale = useMemo(
    () => buildScale(tokenHours.cells.map((cell) => cell.value)),
    [tokenHours],
  )
  const historyLoading = tokens == null && tokensError == null
  const historyCards = (
    <div className="flex flex-col gap-3">
      <HeatmapCard
        title="Tokens by day"
        description={
          tokensError
            ? `history unavailable — ${tokensError}`
            : `${fmtCount(tokenTotals.total)} tokens · ${fmtCount(
                tokenTotals.requests,
              )} requests over ${historyWindow} days`
        }
        rowLabels={tokenDays.rowLabels}
        columnLabels={tokenDays.columnLabels}
        cells={tokenDays.cells}
        scale={dayScale}
        format={(value) => fmtCount(value)}
        unit="tokens"
        cellRole="in that window"
        cellMaxPx={13}
        rotateColumnLabels
        compact
        loading={historyLoading}
      />
      <HeatmapCard
        title="Tokens by hour"
        description="weekday × hour of day, your local time"
        rowLabels={tokenHours.rowLabels}
        columnLabels={tokenHours.columnLabels}
        cells={tokenHours.cells}
        scale={hourScale}
        format={(value) => fmtCount(value)}
        unit="tokens"
        cellRole="in that hour"
        cellMaxPx={16}
        compact
        loading={historyLoading}
      />
    </div>
  )

  const storageCard = (
    <Card aria-busy={loading || undefined}>
      <CardHeader>
        <CardTitle>Storage</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        {loading ? (
          Array.from({ length: 4 }, (_, index) => (
            <div key={index} className="flex flex-col gap-1">
              <div className="flex items-baseline justify-between gap-2">
                <Skeleton className="h-3 w-16" />
                <Skeleton className="h-3 w-24" />
              </div>
              <Skeleton className="h-1.5 w-full rounded-full" />
            </div>
          ))
        ) : (
          <>
        {(latestHost?.disks ?? []).map((disk) => {
          const inodePct =
            disk.inodes_total != null &&
            disk.inodes_total > 0 &&
            disk.inodes_used != null
              ? (disk.inodes_used / disk.inodes_total) * 100
              : null
          return (
            <div key={disk.mount} className="flex flex-col gap-1.5">
              <Meter
                label={disk.mount}
                pct={disk.total_bytes > 0 ? (disk.used_bytes / disk.total_bytes) * 100 : null}
                detail={`${fmtBytes(disk.used_bytes)} / ${fmtBytes(disk.total_bytes)}`}
              />
              {inodePct != null ? (
                <Meter
                  label={`${disk.mount} inodes`}
                  pct={inodePct}
                  detail={`${fmtCount(disk.inodes_used)} / ${fmtCount(disk.inodes_total)}`}
                />
              ) : null}
            </div>
          )
        })}
        {memPct != null ? <Meter label="memory" pct={memPct} /> : null}
        {swapPct != null ? <Meter label="swap" pct={swapPct} /> : null}
        {latestHost?.mem_pressure_pct != null ? (
          <Meter label="memory stall" pct={latestHost.mem_pressure_pct} />
        ) : null}
        {latestHost?.dirty_bytes != null || latestHost?.writeback_bytes != null ? (
          <div className="text-muted-foreground text-[11px] tabular-nums">
            dirty {fmtBytes(latestHost?.dirty_bytes)} · writeback{" "}
            {fmtBytes(latestHost?.writeback_bytes)}
          </div>
        ) : null}
        {(latestHost?.disks ?? []).length === 0 && memPct == null ? (
          <div className="text-faint-foreground py-6 text-center text-xs">
            no host telemetry — run bench/machineview_agent.py
          </div>
        ) : null}
          </>
        )}
      </CardContent>
    </Card>
  )

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
            {live.connected && live.intervalMs != null
              ? `streaming every ${live.intervalMs / 1000} s · stored locally by the load balancer`
              : "refreshes every 5 s · stored locally by the load balancer"}
          </span>
        )}
      </div>

      <div className="grid grid-cols-2 gap-3 md:grid-cols-4 xl:grid-cols-9">
        <StatTile
          label="Gen tok/s"
          value={fmtNum(genTpsAvg)}
          detail="30s avg"
          trend={sparkline(servingRows, "gen_tps", servingNow, sparkWindowMs)}
          format={fmtNum}
          loading={loading}
        />
        <StatTile
          label="Stream tok/s"
          value={fmtNum(latestServing?.stream_tps_p50)}
          detail={
            latestServing?.stream_tps_p05 != null
              ? `p5 ${fmtNum(latestServing.stream_tps_p05)}`
              : "per-request p50"
          }
          trend={sparkline(servingRows, "stream_tps_p50", servingNow, sparkWindowMs)}
          format={fmtNum}
          loading={loading}
        />
        <StatTile
          label="TTFT p95"
          value={fmtMs(latestServing?.ttft_p95_ms)}
          trend={sparkline(servingRows, "ttft_p95", servingNow, sparkWindowMs)}
          format={fmtMs}
          loading={loading}
        />
        <StatTile
          label="In flight"
          value={fmtNum(latestServing?.inflight)}
          trend={sparkline(servingRows, "inflight", servingNow, sparkWindowMs)}
          format={fmtNum}
          loading={loading}
        />
        <StatTile
          label="KV cache"
          value={fmtPct(kvAvg)}
          detail={latestEngines.length ? `${latestEngines.length} engines` : undefined}
          loading={loading}
        />
        <StatTile
          label="Cache hit"
          value={fmtPct(cacheHit.value)}
          detail={cacheHit.detail}
          trend={sparkline(cacheHit.rows, CACHE_HIT_KEY, cacheHit.now, sparkWindowMs)}
          format={fmtPct}
          loading={loading}
        />
        <StatTile
          label="CPU"
          value={fmtPct(latestHost?.cpu_pct)}
          detail={
            latestHost?.load1 != null ? `load ${fmtNum(latestHost.load1, 1)}` : undefined
          }
          trend={sparkline(rows, "cpu", rows.at(-1)?.t ?? servingNow, sparkWindowMs)}
          format={fmtPct}
          loading={loading}
        />
        <StatTile
          label="Memory"
          value={fmtBytes(latestHost?.mem_used_bytes)}
          detail={
            latestHost?.mem_total_bytes != null
              ? `of ${fmtBytes(latestHost.mem_total_bytes)}`
              : undefined
          }
          loading={loading}
        />
        <StatTile
          label="Power"
          value={fmtWatts(powerNow)}
          detail={
            latest?.energy != null
              ? fmtWattHours(latest.energy.total_watt_hours)
              : undefined
          }
          trend={sparkline(rows, "watts_gpu", rows.at(-1)?.t ?? servingNow, sparkWindowMs)}
          format={fmtWatts}
          loading={loading}
        />
      </div>

      <TabBar tabs={TABS} active={tab} onSelect={selectTab} />

      {tab === "overview" ? (
        // High-level: one compact serving row plus the GPU row — one screen.
        <>
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-4">
            {[...servingCards.slice(0, 3), cacheHitCard].map((card) => (
              <ChartCard key={card.title} {...card} loading={loading} />
            ))}
          </div>
          {/* The history pair is narrow by design, so it rides beside the
              GPU row rather than taking a tab of its own. */}
          <div
            className={
              loading || gpuCount > 0
                ? "grid grid-cols-1 gap-3 xl:grid-cols-3"
                : "grid grid-cols-1 gap-3"
            }
          >
            {loading || gpuCount > 0 ? (
              <div className="xl:col-span-2">
                {/* Fills the row rather than guessing a height, so it ends
                    level with the two stacked history cards beside it. */}
                <ChartCard {...gpuUtilCard} height="fill" loading={loading} />
              </div>
            ) : null}
            {historyCards}
          </div>
        </>
      ) : null}

      {tab === "serving" ? <ChartGrid cards={servingCards} loading={loading} /> : null}

      {tab === "topology" ? (
        <AdaptiveTopology
          gpus={latest?.gpus ?? []}
          intake={latestHost?.intake_temp_c ?? null}
        />
      ) : null}

      {tab === "gpus" ? (
        <>
          {loading || gpuCount > 0 ? (
            <ChartCard {...gpuUtilCard} loading={loading} />
          ) : null}
          <GpuGrid
            points={points}
            latest={latest}
            rangeSeconds={rangeSeconds}
            loading={loading}
          />
        </>
      ) : null}

      {tab === "system" ? (
        <div className="flex flex-col gap-3">
          <ChartGrid cards={computeCards} loading={loading} />
          <ChartGrid cards={diskCards} loading={loading} />
          <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
            {powerCards.map((card) => (
              <ChartCard key={card.title} {...card} loading={loading} />
            ))}
            {storageCard}
          </div>
        </div>
      ) : null}

      <footer className="text-faint-foreground pb-4 pt-2 text-[11px]">
        Metrics are sampled and stored locally by the ramjet load balancer —
        no Prometheus or Grafana required.
      </footer>
    </div>
  )
}
