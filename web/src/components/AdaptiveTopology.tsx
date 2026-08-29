import { useCallback, useEffect, useMemo, useState } from "react"
import { ArrowRight, Gauge, LockKeyhole, TimerReset, Wind } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import {
  fetchAdaptiveStatus,
  setAdaptiveMode,
  startAdaptiveTransition,
  type AdaptiveMode,
  type AdaptiveProfile,
  type AdaptiveStatus,
  type GpuSample,
} from "@/lib/api"

const modes: AdaptiveMode[] = ["off", "manual", "recommend", "auto"]

function duration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.round(seconds / 60)
  return `${minutes} min`
}

function metricLabel(metric: string): string {
  return metric.replace("requests_per_second", "req/s").replaceAll("_", " ")
}

function TurbineDiagram({
  profile,
  gpus,
  intake,
  phase,
}: {
  profile: AdaptiveProfile
  gpus: GpuSample[]
  intake: number | null
  phase: string
}) {
  const gpuToEngine = new Map<number, number>()
  profile.engines.forEach((engine, engineIndex) =>
    engine.gpus.forEach((gpu) => gpuToEngine.set(gpu, engineIndex)),
  )
  const busy = phase !== "idle" && phase !== "failed"
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
        />
      ))}

      <g transform="translate(110 215)">
        <circle r="74" fill="var(--card)" stroke="var(--border)" strokeWidth="2" />
        <circle r="57" fill="url(#core)" opacity=".7" filter="url(#glow)" />
        <g className="fan-spin">
          {Array.from({ length: 12 }, (_, index) => (
            <path key={index} d="M0 -15 C19 -46 38 -55 50 -47 C30 -25 24 -8 10 3Z" fill="var(--primary)" opacity={0.25 + (index % 3) * 0.16} transform={`rotate(${index * 30})`} />
          ))}
        </g>
        <circle r="12" fill="var(--foreground)" />
        <text y="108" textAnchor="middle" fill="var(--muted-foreground)" fontSize="13">CHASSIS INTAKE</text>
        <text y="132" textAnchor="middle" fill="var(--foreground)" fontSize="25" fontWeight="650">{intake == null ? "—" : `${intake.toFixed(0)}°C`}</text>
      </g>

      <g transform="translate(270 92)">
        {Array.from({ length: 8 }, (_, gpuIndex) => {
          const x = (gpuIndex % 4) * 142
          const y = Math.floor(gpuIndex / 4) * 132
          const engineIndex = gpuToEngine.get(gpuIndex) ?? 0
          const gpu = gpus.find((sample) => sample.index === gpuIndex)
          const util = gpu?.util_pct ?? 0
          const hue = engineIndex === 0 ? "#00d5ff" : "#a855f7"
          return (
            <g key={gpuIndex} transform={`translate(${x} ${y})`}>
              <rect width="124" height="104" rx="14" fill="var(--card)" stroke={hue} strokeOpacity=".62" strokeWidth="2" />
              <rect x="10" y="68" width="104" height="8" rx="4" fill="var(--muted)" />
              <rect x="10" y="68" width={104 * Math.min(util, 100) / 100} height="8" rx="4" fill={hue} opacity=".9" />
              <circle cx="23" cy="25" r="8" fill={hue} opacity={util > 1 ? ".9" : ".24"} className={util > 1 ? "core-pulse" : ""} />
              <text x="40" y="30" fill="var(--foreground)" fontSize="14" fontWeight="650">GPU {gpuIndex}</text>
              <text x="10" y="55" fill="var(--muted-foreground)" fontSize="12">{util.toFixed(0)}% · {gpu?.temp_c == null ? "—" : `${gpu.temp_c.toFixed(0)}°C`}</text>
              <text x="10" y="94" fill={hue} fontSize="10" letterSpacing=".08em">ENGINE {engineIndex + 1}</text>
            </g>
          )
        })}
      </g>

      <g transform="translate(872 215)">
        <circle r="92" fill="var(--card)" stroke="var(--border)" strokeWidth="2" />
        <circle r="72" fill="url(#core)" opacity=".45" />
        <text y="-12" textAnchor="middle" fill="var(--muted-foreground)" fontSize="12" letterSpacing=".12em">THRUST MODE</text>
        <text y="18" textAnchor="middle" fill="var(--foreground)" fontSize="24" fontWeight="700">{profile.engines.length === 1 ? "TP8" : `${profile.engines.length} × TP4`}</text>
        <text y="43" textAnchor="middle" fill="var(--primary)" fontSize="12">{phase.replaceAll("_", " ").toUpperCase()}</text>
      </g>
      <path d="M970 180 L1080 215 L970 250Z" fill="url(#flow)" opacity=".8" filter="url(#glow)" />
    </svg>
  )
}

export function AdaptiveTopology({
  gpus,
  intake,
}: {
  gpus: GpuSample[]
  intake: number | null
}) {
  const [status, setStatus] = useState<AdaptiveStatus | null | undefined>()
  const [token, setToken] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [working, setWorking] = useState(false)
  const refresh = useCallback(async () => {
    try {
      setStatus(await fetchAdaptiveStatus())
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
  const changeMode = async (mode: AdaptiveMode) => {
    setWorking(true)
    try {
      setStatus(await setAdaptiveMode(mode, token))
      setError(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setWorking(false)
    }
  }
  const transitionTo = async (profile: AdaptiveProfile) => {
    if (!status) return
    const edge = status.transitions.find(
      (transition) => transition.from === status.active_profile && transition.to === profile.id,
    )
    if (!edge) return
    const warning = edge.requires_downtime
      ? `This drains traffic and is expected to take about ${duration(edge.estimated_downtime_seconds)}. Continue?`
      : "Switch engine topology now?"
    if (!window.confirm(warning)) return
    setWorking(true)
    try {
      setStatus(await startAdaptiveTransition(profile.id, token))
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
      {active ? <TurbineDiagram profile={active} gpus={gpus} intake={intake} phase={status.phase} /> : null}
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
                      <Button disabled={working || status.phase !== "idle" || status.mode === "off" || !token} onClick={() => void transitionTo(profile)}>
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
          <CardHeader><CardTitle>Flight control</CardTitle><CardDescription>Auto acts only on transitions explicitly marked automatic.</CardDescription></CardHeader>
          <CardContent className="flex flex-col gap-4">
            <div className="grid grid-cols-4 rounded-lg bg-muted p-1">
              {modes.map((mode) => <Button key={mode} variant="segment" data-active={status.mode === mode} disabled={working || !token} onClick={() => void changeMode(mode)}>{mode}</Button>)}
            </div>
            <label className="flex flex-col gap-1.5 text-xs text-muted-foreground">
              <span className="flex items-center gap-1"><LockKeyhole className="size-3" /> Admin bearer · held only in this page</span>
              <input type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} placeholder="RJ_UPSTREAM_TOKEN" className="h-8 rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground outline-none focus:ring-2 focus:ring-ring/40" />
            </label>
            <div className="grid grid-cols-3 gap-2">
              <div className="rounded-lg bg-muted p-2"><Wind className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.requests_per_second.toFixed(1)}</div><div className="text-[10px] text-muted-foreground">req/s</div></div>
              <div className="rounded-lg bg-muted p-2"><Gauge className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.inflight}</div><div className="text-[10px] text-muted-foreground">in flight</div></div>
              <div className="rounded-lg bg-muted p-2"><Gauge className="mb-1 size-3.5 text-primary" /><div className="text-lg font-semibold tabular-nums">{status.signal.load_per_engine.toFixed(1)}</div><div className="text-[10px] text-muted-foreground">load / engine</div></div>
            </div>
            {status.transitions.filter((item) => item.from === status.active_profile && item.condition).map((item) => <div key={item.to} className="rounded-lg border border-border p-2 text-[11px] text-muted-foreground">Auto → <span className="text-foreground">{item.to}</span> when {metricLabel(item.condition!.metric)} is {item.condition!.comparison} {item.condition!.threshold} for {duration(item.condition!.for_seconds)}</div>)}
            {status.recommendation ? <div className="rounded-lg border border-primary/30 bg-primary/5 p-2 text-xs">Recommendation: switch to <strong>{status.recommendation}</strong></div> : null}
            {status.last_error ? <div className="rounded-lg border border-red-500/30 bg-red-500/5 p-2 text-xs text-red-500">{status.last_error}</div> : null}
            {error ? <div className="text-xs text-red-500">{error}</div> : null}
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
