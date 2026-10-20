import { useMemo, useState } from "react"
import { BucketChart } from "@/components/BucketChart"
import { Segmented as SegmentedGroup, SelectField } from "@/components/Controls"
import { PrincipalPicker } from "@/components/PrincipalPicker"
import {
  BreakdownTable,
  DEFAULT_RANGE,
  KpiTiles,
  METRICS,
  PCT_TICKS,
  RangeControls,
  RecentRequests,
  buildView,
  ms,
  pct,
  rangeMsOf,
  tps,
  windowFor,
} from "@/components/UsageParts"
import { Button } from "@/components/ui/button"
import { usePoll } from "@/hooks/usePoll"
import {
  fetchUsageMeta,
  fetchUsageSeries,
  localTzOffsetMin,
  type Granularity,
  type GroupBy,
  type SeriesResponse,
  type UsageMeta,
} from "@/lib/accounting"
import { seriesFor, type Group, type Metric } from "@/lib/usage"
import { fmtCount } from "@/lib/format"

/**
 * Busiest principals plotted individually. Seven plus the server's exact
 * `__other` fold fills the eight categorical colors.
 */
const CHART_LIMIT = 7

export function UsageTab({
  tokenFilter,
  onTokenFilter,
  onUnauthorized,
}: {
  tokenFilter: string
  onTokenFilter: (token: string) => void
  onUnauthorized: () => void
}) {
  const [granularity, setGranularity] = useState<Granularity>("hour")
  const [range, setRange] = useState(DEFAULT_RANGE.hour)
  const [groupBy, setGroupBy] = useState<GroupBy>("token")
  const [modelFilter, setModelFilter] = useState("")
  const tz = useMemo(() => localTzOffsetMin(), [])

  const meta = usePoll(fetchUsageMeta, "meta", 60_000, onUnauthorized)
  const rangeMs = rangeMsOf(granularity, range)
  const seriesKey = [granularity, range, groupBy, tokenFilter, modelFilter].join("|")
  const series = usePoll(
    () =>
      fetchUsageSeries({
        granularity,
        ...windowFor(granularity, rangeMs, tz),
        group_by: groupBy,
        token: tokenFilter || undefined,
        model: modelFilter || undefined,
        tz_offset_min: tz,
        limit: groupBy === "token" && !tokenFilter ? CHART_LIMIT : undefined,
      }),
    seriesKey,
    granularity === "minute" ? 15_000 : 60_000,
    onUnauthorized,
  )

  const metaData = meta.data
  const response = series.data
  const loading = series.loading && response == null
  const modelOptions = useMemo(
    () => [
      { value: "", label: "all models" },
      ...(metaData?.models ?? []).map((model) => ({ value: model, label: model })),
    ],
    [metaData],
  )

  return (
    <div className="flex flex-col gap-3">
      {/* One filter row — everything below renders against this slice. */}
      <div className="flex flex-wrap items-center gap-2">
        <RangeControls
          granularity={granularity}
          range={range}
          onGranularity={setGranularity}
          onRange={setRange}
        />
        <Segmented3 value={groupBy} onChange={setGroupBy} />
        <PrincipalPicker
          principals={metaData?.principals ?? []}
          value={tokenFilter}
          onChange={onTokenFilter}
        />
        <SelectField label="model" value={modelFilter} options={modelOptions} onChange={setModelFilter} />
        {tokenFilter || modelFilter ? (
          <Button
            variant="ghost"
            onClick={() => {
              onTokenFilter("")
              setModelFilter("")
            }}
          >
            clear filters
          </Button>
        ) : null}
        <span className="text-faint-foreground ml-auto text-[11px]">
          {series.error ? (
            <span style={{ color: "var(--status-critical)" }}>⚠ {series.error}</span>
          ) : (
            `refreshes every ${granularity === "minute" ? "15 s" : "60 s"} · day buckets start at local midnight`
          )}
        </span>
      </div>

      <UsageBody
        response={response}
        meta={metaData}
        loading={loading}
        granularity={granularity}
        tz={tz}
        onSelectKey={(key) => {
          if (groupBy === "token") onTokenFilter(key)
          if (groupBy === "model") setModelFilter(key)
        }}
      />

      <RecentRequests
        meta={metaData}
        tokenFilter={tokenFilter}
        modelFilter={modelFilter}
        onUnauthorized={onUnauthorized}
      />
    </div>
  )
}

function Segmented3({ value, onChange }: { value: GroupBy; onChange: (value: GroupBy) => void }) {
  return (
    <SegmentedGroup
      label="Group by"
      value={value}
      onChange={onChange}
      options={[
        { value: "token", label: "by token" },
        { value: "model", label: "by model" },
        { value: "none", label: "total" },
      ]}
    />
  )
}

function UsageBody({
  response,
  meta,
  loading,
  granularity,
  tz,
  onSelectKey,
}: {
  response: SeriesResponse | null
  meta: UsageMeta | null
  loading: boolean
  granularity: Granularity
  tz: number
  onSelectKey: (key: string) => void
}) {
  // Plot at the granularity of the data actually shown, not of a control
  // that already moved on while the next response is in flight.
  const shownGranularity = response?.granularity ?? granularity
  const view = useMemo(() => (response ? buildView(response, meta, tz) : null), [response, meta, tz])
  const overall = response?.overall
  const groups: Group[] = view?.groups ?? []
  const chart = (
    title: string,
    description: string,
    metric: Metric,
    kind: "bar" | "line",
    format: (value: number) => string,
    options: { domain?: [number | "auto", number | "auto"]; ticks?: number[]; exclude?: string[] } = {},
  ) => {
    const data = view ? view.rows(metric) : []
    // A group with no value anywhere in this chart (e.g. rejected requests
    // have no TTFT) would only add a dead legend entry.
    const plotted = seriesFor(groups, metric).filter(
      (def) =>
        !options.exclude?.includes(def.key) &&
        (metric.kind === "count" || data.some((row) => typeof row[def.key] === "number")),
    )
    return (
      <BucketChart
        key={title}
        title={title}
        description={description}
        data={data}
        series={plotted}
        kind={kind}
        granularity={shownGranularity}
        format={format}
        domain={options.domain}
        ticks={options.ticks}
        loading={loading}
      />
    )
  }

  return (
    <>
      <KpiTiles overall={overall} overallRows={view?.overallRows ?? null} loading={loading} />

      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        {chart("Requests", "per bucket, stacked by group", METRICS.requests, "bar", (v) => fmtCount(v))}
        {chart(
          "Tokens",
          "prompt + completion per bucket, stacked by group",
          METRICS.tokens,
          "bar",
          (v) => fmtCount(v),
        )}
        <BucketChart
          title="Token mix"
          description="all groups · cached prompt tokens were served from prefix cache"
          data={view?.overallRows ?? []}
          series={[
            { key: "cached", label: "cached prompt", color: "var(--chart-1)" },
            { key: "uncached", label: "uncached prompt", color: "var(--chart-4)" },
            { key: "completion", label: "completion", color: "var(--chart-3)" },
          ]}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
        {chart(
          "Cache hit ratio",
          "cached ÷ prompt tokens; gaps are buckets with no prompts",
          METRICS.cacheHit,
          "line",
          pct,
          { domain: [0, 100], ticks: PCT_TICKS },
        )}
        {chart("TTFT p50", "time to first token, median", METRICS.ttftP50, "line", ms)}
        {chart("TTFT p90", "time to first token, 90th percentile", METRICS.ttftP90, "line", ms)}
        {chart("Decode tok/s p50", "per-request decode rate, median", METRICS.tpsP50, "line", tps)}
        {chart("Duration p90", "request wall time, 90th percentile", METRICS.durationP90, "line", ms)}
        {chart(
          "Error rate",
          response?.group_by === "token"
            ? "4xx + 5xx share of requests · rejected (always 100%) omitted"
            : "4xx + 5xx share of requests",
          METRICS.errorRate,
          "line",
          pct,
          { exclude: response?.group_by === "token" ? ["rejected"] : [] },
        )}
        <BucketChart
          title="Errors"
          description="all groups, by class"
          data={view?.overallRows ?? []}
          series={[
            { key: "client", label: "4xx", color: "var(--status-warning)" },
            { key: "server", label: "5xx", color: "var(--status-critical)" },
            { key: "disconnects", label: "client disconnects", color: "var(--faint-foreground)" },
          ]}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
        <BucketChart
          title="Upstream split"
          description="requests per engine, all groups"
          data={view?.upstreamRows ?? []}
          series={view?.upstreamSeries ?? []}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
        <BucketChart
          title="Cache outcomes"
          description="requests by prefix-cache result, all groups"
          data={view?.overallRows ?? []}
          series={[
            { key: "full", label: "full", color: "var(--chart-1)" },
            { key: "partial", label: "partial", color: "var(--chart-2)" },
            { key: "cold", label: "cold", color: "var(--chart-4)" },
            { key: "unknown", label: "unknown", color: "var(--faint-foreground)" },
          ]}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
      </div>

      <BreakdownTable
        response={response}
        meta={meta}
        groups={groups}
        loading={loading}
        onSelectKey={onSelectKey}
      />
    </>
  )
}
