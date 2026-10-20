import { useMemo, useState } from "react"
import { ArrowLeft, BarChart3, KeyRound, ShieldAlert, User, UserX } from "lucide-react"
import { BucketChart, type BucketRow, type BucketSeries } from "@/components/BucketChart"
import { SelectField } from "@/components/Controls"
import {
  ExpiryCell,
  StatusBadge,
  TokenActionButtons,
  TokenDialogs,
  When,
  type TokenDialogState,
} from "@/components/TokensTab"
import {
  BreakdownTable,
  FinishReasonsCard,
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
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { usePoll } from "@/hooks/usePoll"
import {
  FIXED_PRINCIPALS,
  fetchUsageMeta,
  fetchUsageSeries,
  getToken,
  localTzOffsetMin,
  tokenStatusOf,
  type Granularity,
  type SeriesResponse,
  type Stats,
} from "@/lib/accounting"
import { indexSeries, responseGrid, seriesFor } from "@/lib/usage"
import { fmtCount } from "@/lib/format"

const DAY = 86_400_000

const FIXED_COPY: Record<string, { name: string; about: string }> = {
  master: {
    name: "master token",
    about: "Requests that authenticated with the engine (master) token itself.",
  },
  anonymous: {
    name: "anonymous",
    about:
      "Requests that carried no recognised credential while RJ_API_TOKENS_MODE was off; they were still served.",
  },
  rejected: {
    name: "rejected",
    about: "Requests ramjet answered with 401 itself because authentication failed. They never reached an engine.",
  },
}

/** Per-bucket quantile columns from the `group_by=none` response. */
function quantileRows(
  response: SeriesResponse,
  tz: number,
  columns: Record<string, (stats: Stats) => number | null>,
): BucketRow[] {
  const grid = responseGrid(response, tz)
  const points = indexSeries(response).get("all") ?? new Map()
  return grid.map((t) => {
    const row: BucketRow = { t }
    const stats = points.get(t)
    for (const [key, get] of Object.entries(columns)) row[key] = stats ? get(stats) : null
    return row
  })
}

const QUANTILE_COLORS = ["var(--chart-1)", "var(--chart-3)", "var(--chart-5)"]

export function TokenDetail({
  id,
  onUnauthorized,
  onOpenUsage,
}: {
  id: string
  onUnauthorized: () => void
  onOpenUsage: (id: string) => void
}) {
  const [granularity, setGranularity] = useState<Granularity>("hour")
  const [range, setRange] = useState("7d")
  const [modelFilter, setModelFilter] = useState("")
  const [dialog, setDialog] = useState<TokenDialogState>(null)
  const tz = useMemo(() => localTzOffsetMin(), [])

  const meta = usePoll(fetchUsageMeta, "meta", 60_000, onUnauthorized)
  const principal = meta.data?.principals.find((candidate) => candidate.key === id)
  const fixed = FIXED_PRINCIPALS.has(id)
  // Deleted tokens and fixed principals have no token record to fetch.
  const tokenLike = !fixed && principal?.kind !== "deleted_token"
  const token = usePoll(
    () => (tokenLike ? getToken(id) : Promise.resolve(null)),
    `token:${id}:${tokenLike}`,
    30_000,
    onUnauthorized,
  )

  const rangeMs = rangeMsOf(granularity, range)
  const key = [id, granularity, range, modelFilter].join("|")
  const params = () => ({
    granularity,
    ...windowFor(granularity, rangeMs, tz),
    token: id,
    model: modelFilter || undefined,
    tz_offset_min: tz,
  })
  const refreshMs = granularity === "minute" ? 15_000 : 60_000
  const byModel = usePoll(
    () => fetchUsageSeries({ ...params(), group_by: "model" }),
    `model|${key}`,
    refreshMs,
    onUnauthorized,
  )
  const total = usePoll(
    () => fetchUsageSeries({ ...params(), group_by: "none" }),
    `none|${key}`,
    refreshMs,
    onUnauthorized,
  )

  const modelResponse = byModel.data
  const totalResponse = total.data
  const loading = (byModel.loading && modelResponse == null) || (total.loading && totalResponse == null)
  const shownGranularity = totalResponse?.granularity ?? granularity
  const view = useMemo(
    () => (modelResponse ? buildView(modelResponse, meta.data, tz) : null),
    [modelResponse, meta.data, tz],
  )
  const quantiles = useMemo(
    () =>
      totalResponse
        ? quantileRows(totalResponse, tz, {
            ttft50: (s) => s.ttft_p50_ms,
            ttft90: (s) => s.ttft_p90_ms,
            ttft99: (s) => s.ttft_p99_ms,
            tps10: (s) => s.decode_tps_p10,
            tps50: (s) => s.decode_tps_p50,
            dur50: (s) => s.duration_p50_ms,
            dur90: (s) => s.duration_p90_ms,
            hit: (s) => (s.cache_hit_ratio == null ? null : s.cache_hit_ratio * 100),
          })
        : [],
    [totalResponse, tz],
  )
  const overall = totalResponse?.overall
  const groups = view?.groups ?? []
  const lines = (defs: Array<[string, string]>): BucketSeries[] =>
    defs.map(([key, label], index) => ({ key, label, color: QUANTILE_COLORS[index] }))

  const tokenData = token.data
  const now = Date.now()
  const status = tokenData ? tokenStatusOf(tokenData, now) : null
  const name = tokenData?.name ?? FIXED_COPY[id]?.name ?? principal?.name ?? id
  const modelOptions = [
    { value: "", label: "all models" },
    ...(meta.data?.models ?? []).map((model) => ({ value: model, label: model })),
  ]
  const kind = fixed ? id : principal?.kind === "deleted_token" ? "deleted token" : "token"
  const Icon = id === "rejected" ? ShieldAlert : id === "anonymous" ? UserX : fixed ? User : KeyRound

  return (
    <div className="flex flex-col gap-3">
      <a
        href="#tokens"
        className="text-muted-foreground hover:text-foreground inline-flex items-center gap-1 self-start text-xs"
      >
        <ArrowLeft className="size-3.5" /> all tokens
      </a>

      <Card>
        <CardContent className="flex flex-wrap items-start justify-between gap-4 pt-4">
          <div className="flex min-w-0 gap-3">
            <span className="flex size-10 shrink-0 items-center justify-center rounded-lg border border-border bg-muted">
              <Icon className="size-4" />
            </span>
            <div className="flex min-w-0 flex-col gap-1">
              <div className="flex flex-wrap items-center gap-2">
                <h2 className="truncate text-lg font-semibold tracking-tight">{name}</h2>
                {status ? <StatusBadge status={status} /> : <Badge variant="outline">{kind}</Badge>}
              </div>
              <div className="text-muted-foreground text-xs">
                {tokenData
                  ? tokenData.description || <span className="text-faint-foreground">no description</span>
                  : FIXED_COPY[id]?.about ??
                    (principal?.kind === "deleted_token"
                      ? "This token was deleted. Its usage history is kept under its id."
                      : token.loading
                        ? " "
                        : token.error
                          ? `No token record: ${token.error}`
                          : " ")}
              </div>
              {tokenLike || principal?.kind === "deleted_token" ? (
                <div className="font-mono text-[11px]">
                  {tokenData?.prefix ?? `rjk_${id}`}
                  <span className="text-faint-foreground">_…</span>
                </div>
              ) : null}
              {tokenData && status ? (
                <dl className="mt-1 grid grid-cols-2 gap-x-6 gap-y-1 text-[11px] sm:grid-cols-4">
                  <div>
                    <dt className="text-faint-foreground">created</dt>
                    <dd><When t={tokenData.created_at_ms} now={now} /></dd>
                  </div>
                  <div>
                    <dt className="text-faint-foreground">{status === "revoked" ? "revoked" : "expires"}</dt>
                    <dd>
                      <ExpiryCell
                        token={tokenData}
                        status={status}
                        soon={
                          status === "active" &&
                          tokenData.expires_at_ms != null &&
                          tokenData.expires_at_ms - now < 7 * DAY
                        }
                        now={now}
                      />
                    </dd>
                  </div>
                  <div>
                    <dt className="text-faint-foreground">last used</dt>
                    <dd><When t={tokenData.last_used_ms} now={now} empty="never" /></dd>
                  </div>
                  <div>
                    <dt className="text-faint-foreground">id</dt>
                    <dd className="font-mono">{tokenData.id}</dd>
                  </div>
                </dl>
              ) : null}
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Button variant="ghost" onClick={() => onOpenUsage(id)}>
              <BarChart3 /> compare in Usage
            </Button>
            {tokenData && status ? (
              <TokenActionButtons
                token={tokenData}
                status={status}
                labels
                onEdit={(target) => setDialog({ kind: "edit", token: target })}
                onRevoke={(target) => setDialog({ kind: "revoke", token: target })}
                onDelete={(target) => setDialog({ kind: "delete", token: target })}
              />
            ) : null}
          </div>
        </CardContent>
      </Card>

      <div className="flex flex-wrap items-center gap-2">
        <RangeControls
          granularity={granularity}
          range={range}
          onGranularity={setGranularity}
          onRange={setRange}
        />
        <SelectField label="model" value={modelFilter} options={modelOptions} onChange={setModelFilter} />
        <span className="text-faint-foreground ml-auto text-[11px]">
          {byModel.error || total.error ? (
            <span style={{ color: "var(--status-critical)" }}>⚠ {byModel.error ?? total.error}</span>
          ) : overall ? (
            `${fmtCount(overall.requests)} requests in range · refreshes every ${refreshMs / 1000} s`
          ) : null}
        </span>
      </div>

      <KpiTiles overall={overall} overallRows={view?.overallRows ?? null} loading={loading} />

      <div className="grid grid-cols-1 gap-3 md:grid-cols-2 xl:grid-cols-3">
        <BucketChart
          title="Requests"
          description="per bucket, stacked by model"
          data={view ? view.rows(METRICS.requests) : []}
          series={seriesFor(groups, METRICS.requests)}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
        <BucketChart
          title="Tokens"
          description="cached prompt tokens were served from prefix cache"
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
        <BucketChart
          title="Cache hit ratio"
          description="cached ÷ prompt tokens, per model; gaps are buckets with no prompts"
          data={view ? view.rows(METRICS.cacheHit) : []}
          series={seriesFor(groups, METRICS.cacheHit)}
          kind="line"
          granularity={shownGranularity}
          format={pct}
          domain={[0, 100]}
          ticks={PCT_TICKS}
          loading={loading}
        />
        <BucketChart
          title="TTFT"
          description="time to first token percentiles"
          data={quantiles}
          series={lines([
            ["ttft50", "p50"],
            ["ttft90", "p90"],
            ["ttft99", "p99"],
          ])}
          kind="line"
          granularity={shownGranularity}
          format={ms}
          loading={loading}
        />
        <BucketChart
          title="Decode tok/s"
          description="per-request decode rate · p10 is the slow tail a user feels"
          data={quantiles}
          series={lines([
            ["tps50", "p50"],
            ["tps10", "p10"],
          ])}
          kind="line"
          granularity={shownGranularity}
          format={tps}
          loading={loading}
        />
        <BucketChart
          title="Duration"
          description="request wall time percentiles"
          data={quantiles}
          series={lines([
            ["dur50", "p50"],
            ["dur90", "p90"],
          ])}
          kind="line"
          granularity={shownGranularity}
          format={ms}
          loading={loading}
        />
        <BucketChart
          title="Errors"
          description="per bucket, by class"
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
          description="requests per engine"
          data={view?.upstreamRows ?? []}
          series={view?.upstreamSeries ?? []}
          kind="bar"
          granularity={shownGranularity}
          format={(v) => fmtCount(v)}
          loading={loading}
        />
        <FinishReasonsCard stats={overall} loading={loading} />
      </div>

      {loading && modelResponse == null ? (
        <Skeleton className="h-24 w-full" />
      ) : (
        <BreakdownTable
          response={modelResponse}
          meta={meta.data}
          groups={groups}
          loading={loading}
          onSelectKey={setModelFilter}
        />
      )}

      <RecentRequests
        meta={meta.data}
        tokenFilter={id}
        modelFilter={modelFilter}
        onUnauthorized={onUnauthorized}
        fixedToken
      />

      <TokenDialogs
        dialog={dialog}
        onClose={() => setDialog(null)}
        onUnauthorized={onUnauthorized}
        onChanged={() => {
          token.refresh()
          meta.refresh()
        }}
        onDeleted={() => {
          window.location.hash = "#tokens"
        }}
      />
    </div>
  )
}
