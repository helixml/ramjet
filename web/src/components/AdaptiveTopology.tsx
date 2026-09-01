import { useCallback, useEffect, useMemo, useState } from "react"
import { Activity, AlertTriangle, ArrowDownToLine, ArrowRight, ArrowUpFromLine, Gauge, History, RotateCcw, TimerReset } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { TransitionDialog } from "@/components/TransitionDialog"
import {
  fetchAdaptiveAudit,
  fetchAdaptiveStatus,
  retryAdaptiveRollback,
  setAdaptiveMode,
  startAdaptiveTransition,
  type AdaptiveAuditRecord,
  type AdaptiveMode,
  type AdaptiveProfile,
  type AdaptiveStatus,
  type GpuSample,
} from "@/lib/api"
import { GPU_UTIL_WINDOW_MS } from "@/lib/gpus"

const modes: AdaptiveMode[] = ["off", "manual", "recommend", "auto"]

function duration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.round(seconds / 60)
  return `${minutes} min`
}

function metricLabel(metric: string): string {
  return metric
    .replace("prompt_tokens_per_second", "input tok/s")
    .replace("completion_tokens_per_second", "output tok/s")
    .replace("tokens_per_second", "total tok/s")
    .replace("requests_per_second", "req/s")
    .replaceAll("_", " ")
}

function rate(value: number): string {
  if (value >= 1000) return `${(value / 1000).toFixed(value >= 10_000 ? 0 : 1)}k`
  return value.toFixed(value >= 100 ? 0 : 1)
}

function TokenFlowDiagram({
  profile,
  gpus,
  signal,
  phase,
}: {
  profile: AdaptiveProfile
  gpus: GpuSample[]
  signal: AdaptiveStatus["signal"]
  phase: string
}) {
  const gpuToEngine = new Map<number, number>()
  profile.engines.forEach((engine, engineIndex) =>
    engine.gpus.forEach((gpu) => gpuToEngine.set(gpu, engineIndex)),
  )
  const busy = phase !== "idle" && phase !== "failed"
  const flowSeconds = Math.max(0.7, Math.min(5, 5 - Math.log10(signal.tokens_per_second + 1)))
  return (
    <svg
      viewBox="0 0 1120 430"
      role="img"
      aria-label={`${profile.label}, ${profile.engines.length} engine configuration`}
      className="adaptive-turbine h-auto w-full"
    >
      <defs>
        <linearGradient id="duct" x1="0" x2="1">
          <stop offset="0" stopColor="var(--card)" />
          <stop offset=".42" stopColor="var(--muted)" />
          <stop offset="1" stopColor="var(--card)" />
        </linearGradient>
        <linearGradient id="flow" x1="0" x2="1">
          <stop offset="0" stopColor="#22d3ee" stopOpacity=".05" />
          <stop offset=".45" stopColor="#00d5ff" stopOpacity=".9" />
          <stop offset="1" stopColor="#a855f7" stopOpacity=".15" />
        </linearGradient>
        <radialGradient id="core">
          <stop offset="0" stopColor="#fff" stopOpacity=".92" />
          <stop offset=".2" stopColor="#67e8f9" stopOpacity=".82" />
          <stop offset="1" stopColor="#0891b2" stopOpacity=".08" />
        </radialGradient>
        <filter id="glow" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="7" result="blur" />
          <feMerge><feMergeNode in="blur" /><feMergeNode in="SourceGraphic" /></feMerge>
        </filter>
        <pattern id="grid" width="24" height="24" patternUnits="userSpaceOnUse">
          <path d="M24 0H0V24" fill="none" stroke="var(--grid)" strokeWidth="1" />
        </pattern>
      </defs>

      <rect x="1" y="1" width="1118" height="428" rx="24" fill="url(#grid)" stroke="var(--border)" />
      <path d="M48 118 C170 118 192 55 315 55 H1028 L1090 215 L1028 375 H315 C192 375 170 312 48 312Z" fill="url(#duct)" stroke="var(--border)" strokeWidth="2" />
      {[0, 1, 2, 3, 4].map((line) => (
        <path
          key={line}
          d={`M38 ${148 + line * 33} C205 ${148 + line * 25}, 245 ${116 + line * 49}, 1080 ${146 + line * 34}`}
          fill="none"
          stroke="url(#flow)"
          strokeWidth={line === 2 ? 4 : 2}
          strokeDasharray="8 18"
          className={busy ? "airflow airflow-slow" : "airflow"}
          style={{ animationDuration: `${flowSeconds + line * 0.08}s` }}
        />
      ))}

      <g transform="translate(110 215)">
        <circle r="74" fill="var(--card)" stroke="var(--border)" strokeWidth="2" />
        <circle r="57" fill="url(#core)" opacity=".7" filter="url(#glow)" />
        <g className="fan-spin" style={{ animationDuration: `${Math.max(1.2, flowSeconds * 1.4)}s` }}>
          {Array.from({ length: 12 }, (_, index) => (
            <path key={index} d="M0 -15 C19 -46 38 -55 50 -47 C30 -25 24 -8 10 3Z" fill="var(--primary)" opacity={0.25 + (index % 3) * 0.16} transform={`rotate(${index * 30})`} />
          ))}
        </g>
        <circle r="12" fill="var(--foreground)" />
        <text y="108" textAnchor="middle" fill="var(--muted-foreground)" fontSize="13">TOKEN INTAKE</text>
        <text y="132" textAnchor="middle" fill="var(--foreground)" fontSize="25" fontWeight="650">{rate(signal.prompt_tokens_per_second)}</text>
        <text y="150" textAnchor="middle" fill="var(--muted-foreground)" fontSize="11">input tok/s</text>
      </g>

      <g transform="translate(270 92)">
        {Array.from({ length: 8 }, (_, gpuIndex) => {
          const x = (gpuIndex % 4) * 142
          const y = Math.floor(gpuIndex / 4) * 132
          const engineIndex = gpuToEngine.get(gpuIndex) ?? 0
          const gpu = gpus.find((sample) => sample.index === gpuIndex)
          const util = gpu?.util_pct
          const utilValue = util ?? 0
          const hue = engineIndex === 0 ? "#00d5ff" : "#a855f7"
          return (
            <g key={gpuIndex} transform={`translate(${x} ${y})`}>
              <rect width="124" height="104" rx="14" fill="var(--card)" stroke={hue} strokeOpacity=".62" strokeWidth="2" />
              <rect x="10" y="68" width="104" height="8" rx="4" fill="var(--muted)" />
              <rect x="10" y="68" width={104 * Math.min(utilValue, 100) / 100} height="8" rx="4" fill={hue} opacity=".9" />
              <circle cx="23" cy="25" r="8" fill={hue} opacity={utilValue > 1 ? ".9" : ".24"} className={utilValue > 1 ? "core-pulse" : ""} />
              <text x="40" y="30" fill="var(--foreground)" fontSize="14" fontWeight="650">GPU {gpuIndex}</text>
              <text x="10" y="55" fill="var(--muted-foreground)" fontSize="12">
                {util == null ? "— UTILIZATION" : `${util.toFixed(0)}% · ${GPU_UTIL_WINDOW_MS / 1000}S AVG`}
              </text>
              <text x="10" y="94" fill={hue} fontSize="10" letterSpacing=".08em">ENGINE {engineIndex + 1}</text>
            </g>
          )
        })}
      </g>

      <g transform="translate(872 215)">
        <circle r="92" fill="var(--card)" stroke="var(--border)" strokeWidth="2" />
        <circle r="72" fill="url(#core)" opacity=".45" />
        <text y="-12" textAnchor="middle" fill="var(--muted-foreground)" fontSize="12" letterSpacing=".12em">ACTIVE TOPOLOGY</text>
        <text y="18" textAnchor="middle" fill="var(--foreground)" fontSize="24" fontWeight="700">{profile.engines.length === 1 ? "TP8" : `${profile.engines.length} × TP4`}</text>
        <text y="43" textAnchor="middle" fill="var(--primary)" fontSize="12">{phase.replaceAll("_", " ").toUpperCase()}</text>
      </g>
      <path d="M970 180 L1080 215 L970 250Z" fill="url(#flow)" opacity=".8" filter="url(#glow)" />
      <text x="1035" y="278" textAnchor="middle" fill="var(--muted-foreground)" fontSize="10" letterSpacing=".1em">OUTPUT</text>
      <text x="1035" y="298" textAnchor="middle" fill="var(--foreground)" fontSize="17" fontWeight="650">{rate(signal.completion_tokens_per_second)} tok/s</text>
      <text x="560" y="355" textAnchor="middle" fill="var(--muted-foreground)" fontSize="12">
        SYSTEM LOAD {signal.load_per_engine.toFixed(1)} / ENGINE · {signal.inflight} IN FLIGHT · {rate(signal.tokens_per_second)} TOTAL TOK/S
      </text>
    </svg>
  )
}

export function AdaptiveTopology({
  gpus,
}: {
  gpus: GpuSample[]
}) {
  const [status, setStatus] = useState<AdaptiveStatus | null | undefined>()
  const [audit, setAudit] = useState<AdaptiveAuditRecord[]>([])
  const [error, setError] = useState<string | null>(null)
  const [working, setWorking] = useState(false)
  const [pendingProfile, setPendingProfile] = useState<AdaptiveProfile | null>(null)
  const [dialogError, setDialogError] = useState<string | null>(null)
  const refresh = useCallback(async () => {
    try {
      const next = await fetchAdaptiveStatus()
      setStatus(next)
      if (next) setAudit(await fetchAdaptiveAudit())
      setError(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }, [])
  useEffect(() => {
    void refresh()
    const timer = window.setInterval(() => void refresh(), 3000)
    return () => window.clearInterval(timer)
  }, [refresh])

  const active = useMemo(
    () => status?.profiles.find((profile) => profile.id === status.active_profile),
    [status],
  )
  const pendingTransition = useMemo(
    () => pendingProfile && status
      ? status.transitions.find((item) => item.from === status.active_profile && item.to === pendingProfile.id)
      : undefined,
    [pendingProfile, status],
  )
  const closeTransitionDialog = useCallback(() => {
    setPendingProfile(null)
    setDialogError(null)
  }, [])
  const changeMode = async (mode: AdaptiveMode) => {
    setWorking(true)
    try {
      setStatus(await setAdaptiveMode(mode))
      setAudit(await fetchAdaptiveAudit())
      setError(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setWorking(false)
    }
  }
  const transitionTo = async (profile: AdaptiveProfile) => {
    if (!status) return
    setWorking(true)
    try {
      setStatus(await startAdaptiveTransition(profile.id))
      setAudit(await fetchAdaptiveAudit())
      setError(null)
      setDialogError(null)
      setPendingProfile(null)
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      setError(message)
      setDialogError(message)
    } finally {
      setWorking(false)
    }
  }
  const retryRollback = async () => {
    setWorking(true)
    try {
      setStatus(await retryAdaptiveRollback())
      setAudit(await fetchAdaptiveAudit())
      setError(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setWorking(false)
    }
  }

  if (status === undefined) {
    return <Card><CardContent className="py-20 text-center text-sm text-muted-foreground">reading engine topology…</CardContent></Card>
  }
  if (status === null) {
    return <Card><CardHeader><CardTitle>Adaptive topology is not configured</CardTitle><CardDescription>Mount a reviewed profile file and set RJ_ADAPTIVE_CONFIG_PATH to enable the embedded controller.</CardDescription></CardHeader></Card>
  }

  return (
    <div className="flex flex-col gap-3">
      {active ? <TokenFlowDiagram profile={active} gpus={gpus} signal={status.signal} phase={status.phase} /> : null}
      {status.target_profile ? (
        <Card className={status.phase === "failed" ? "border-red-500/35 bg-red-500/[0.04]" : "border-primary/25 bg-primary/[0.03]"}>
          <CardContent className="flex flex-col gap-3 py-4 sm:flex-row sm:items-center sm:justify-between">
            <div className="flex min-w-0 gap-3">
              <span className={`flex size-9 shrink-0 items-center justify-center rounded-lg ${status.phase === "failed" ? "bg-red-500/10 text-red-500" : "bg-primary/10 text-primary"}`}>
                {status.phase === "failed" ? <AlertTriangle className="size-4" /> : <RotateCcw className="size-4 animate-spin" />}
              </span>
              <div className="min-w-0">
                <div className="text-sm font-semibold">
                  {status.phase === "failed" ? "Topology recovery required" : `Transition ${status.phase.replaceAll("_", " ")}`}
                </div>
                <p className="mt-1 text-xs leading-5 text-muted-foreground">
                  {status.phase === "failed"
                    ? `Routing is fenced. Restore ${active?.label ?? status.active_profile} from the durable transition journal before attempting another shape.`
                    : `${active?.label ?? status.active_profile} → ${status.profiles.find((profile) => profile.id === status.target_profile)?.label ?? status.target_profile}`}
                </p>
                {status.last_error ? <p className="mt-1 text-[11px] text-red-500">{status.last_error}</p> : null}
              </div>
            </div>
            {status.phase === "failed" ? (
              <Button disabled={working} onClick={() => void retryRollback()} className="shrink-0">
                <RotateCcw /> {working ? "Starting rollback…" : "Retry rollback"}
              </Button>
            ) : null}
          </CardContent>
        </Card>
      ) : null}
      <div className="grid grid-cols-1 gap-3 xl:grid-cols-[1fr_360px]">
        <Card>
          <CardHeader>
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <CardTitle>Engine shapes</CardTitle>
                <CardDescription>Named, immutable configurations; only pre-created containers can be selected.</CardDescription>
              </div>
              <Badge variant="outline" className={status.phase === "failed" ? "text-red-500" : undefined}>{status.phase.replaceAll("_", " ")}</Badge>
            </div>
          </CardHeader>
          <CardContent className="grid grid-cols-1 gap-3 md:grid-cols-2">
            {status.profiles.map((profile) => {
              const edge = status.transitions.find((item) => item.from === status.active_profile && item.to === profile.id)
              return (
                <div key={profile.id} className={`rounded-lg border p-3 ${profile.active ? "border-primary bg-primary/5" : "border-border"}`}>
                  <div className="flex items-start justify-between gap-3">
                    <div><div className="text-sm font-semibold">{profile.label}</div><div className="mt-1 text-xs text-muted-foreground">{profile.description}</div></div>
                    <Badge variant={profile.active ? "default" : "outline"}>{profile.engines.length === 1 ? "TP8" : `${profile.engines.length} engines`}</Badge>
                  </div>
                  <div className="mt-3 flex flex-wrap gap-1.5">
                    {profile.engines.map((engine) => <span key={engine.container} className="rounded bg-muted px-2 py-1 font-mono text-[10px]">{engine.label} · GPU {engine.gpus.join("–")}</span>)}
                  </div>
                  {edge ? (
                    <div className="mt-3 flex items-center justify-between gap-2 border-t border-border pt-3">
                      <div className="text-[11px] text-muted-foreground">
                        {edge.requires_downtime ? <><TimerReset className="mr-1 inline size-3" />~{duration(edge.estimated_downtime_seconds)} downtime</> : "live change"}
                      </div>
                      <Button disabled={working || status.phase !== "idle" || status.mode === "off"} onClick={() => { setDialogError(null); setPendingProfile(profile) }}>
                        Configure <ArrowRight />
                      </Button>
                    </div>
                  ) : null}
                </div>
              )
            })}
          </CardContent>
        </Card>

        <Card>
          <CardHeader><CardTitle>Topology control</CardTitle><CardDescription>Auto evaluates measured token flow and serving load only on explicitly automatic transitions.</CardDescription></CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="grid grid-cols-4 rounded-lg bg-muted p-1">
              {modes.map((mode) => <Button key={mode} variant="segment" data-active={status.mode === mode} disabled={working} onClick={() => void changeMode(mode)}>{mode}</Button>)}
            </div>
            <div className="grid grid-cols-3 gap-2">
              <div className="rounded-lg bg-muted p-2"><ArrowDownToLine className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{rate(status.signal.prompt_tokens_per_second)}</div><div className="text-[10px] text-muted-foreground">input tok/s</div></div>
              <div className="rounded-lg bg-muted p-2"><ArrowUpFromLine className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{rate(status.signal.completion_tokens_per_second)}</div><div className="text-[10px] text-muted-foreground">output tok/s</div></div>
              <div className="rounded-lg bg-muted p-2"><Activity className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{rate(status.signal.tokens_per_second)}</div><div className="text-[10px] text-muted-foreground">total tok/s</div></div>
              <div className="rounded-lg bg-muted p-2"><Gauge className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.inflight}</div><div className="text-[10px] text-muted-foreground">in flight</div></div>
              <div className="rounded-lg bg-muted p-2"><Gauge className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.load_per_engine.toFixed(1)}</div><div className="text-[10px] text-muted-foreground">load / engine</div></div>
              <div className="rounded-lg bg-muted p-2"><Activity className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.requests_per_second.toFixed(1)}</div><div className="text-[10px] text-muted-foreground">req/s</div></div>
            </div>
            {status.transitions.filter((item) => item.from === status.active_profile && item.condition).map((item) => <div key={item.to} className="rounded-lg border border-border p-2 text-[11px] text-muted-foreground">Auto → <span className="text-foreground">{item.to}</span> when {metricLabel(item.condition!.metric)} is {item.condition!.comparison} {item.condition!.threshold} for {duration(item.condition!.for_seconds)}</div>)}
            {status.recommendation ? <div className="rounded-lg border border-primary/30 bg-primary/5 p-2 text-xs">Recommendation: switch to <strong>{status.recommendation}</strong></div> : null}
            {status.last_error ? <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-2 text-xs text-red-500">{status.last_error}</div> : null}
            {error ? <div className="text-xs text-red-500">{error}</div> : null}
          </CardContent>
        </Card>
      </div>
      <Card>
        <CardHeader>
          <div className="flex items-center gap-2">
            <History className="size-4 text-primary" />
            <div>
              <CardTitle>Engine change history</CardTitle>
              <CardDescription>Durable controller and engine actions, newest first.</CardDescription>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {audit.length === 0 ? (
            <div className="py-4 text-center text-xs text-muted-foreground">No engine changes recorded yet.</div>
          ) : (
            <div className="divide-y divide-border">
              {audit.slice(-10).reverse().map((record) => (
                <div key={`${record.timestamp_unix_ms}-${record.event}-${record.engine ?? ""}`} className="grid gap-1 py-2 text-xs md:grid-cols-[160px_180px_1fr] md:items-center">
                  <time className="font-mono text-[11px] text-muted-foreground" dateTime={new Date(record.timestamp_unix_ms).toISOString()}>
                    {new Date(record.timestamp_unix_ms).toLocaleString()}
                  </time>
                  <span className="font-medium">{record.event.replaceAll("_", " ")}</span>
                  <span className="text-muted-foreground">
                    {record.engine ?? record.target_profile ?? record.active_profile}
                    {record.source ? ` · ${record.source}` : ""}
                    {record.detail ? ` · ${record.detail}` : ""}
                  </span>
                </div>
              ))}
            </div>
          )}
        </CardContent>
      </Card>
      {pendingProfile && pendingTransition && active ? (
        <TransitionDialog
          current={active}
          target={pendingProfile}
          transition={pendingTransition}
          working={working}
          error={dialogError}
          onCancel={closeTransitionDialog}
          onConfirm={() => void transitionTo(pendingProfile)}
        />
      ) : null}
    </div>
  )
}
