import { ArrowRight, Boxes, Cpu, Network } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { endpointLabel, fmtNum, fmtPct } from "@/lib/format"
import type { GpuSample, Sample, ServingTopologyEngine } from "@/lib/api"

function engineLetter(index: number): string {
  return String.fromCharCode(65 + index)
}

function healthLabel(up: number | null | undefined): string {
  if (up == null) return "status unknown"
  return up > 0 ? "serving" : "unavailable"
}

export function ServingTopology({
  topology,
  latest,
  gpus,
}: {
  topology: ServingTopologyEngine[]
  latest: Sample | null
  gpus: GpuSample[]
}) {
  return (
    <Card>
      <CardHeader>
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <CardTitle>Live serving topology</CardTitle>
            <CardDescription>
              Model ownership and tensor-parallel GPU groups configured on this Ramjet instance.
            </CardDescription>
          </div>
          <Badge variant="outline">
            {topology.length} {topology.length === 1 ? "TP engine" : "TP engines"}
          </Badge>
        </div>
      </CardHeader>
      <CardContent>
        {topology.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border py-10 text-center text-sm text-muted-foreground">
            No serving upstreams are configured.
          </div>
        ) : (
          <div className="grid items-stretch gap-3 xl:grid-cols-[220px_32px_minmax(0,1fr)]">
            <div className="flex min-h-36 flex-col justify-center rounded-xl border border-primary/25 bg-primary/[0.04] p-4">
              <div className="mb-3 flex size-10 items-center justify-center rounded-lg bg-primary/10 text-primary">
                <Network className="size-5" />
              </div>
              <div className="text-sm font-semibold">Ramjet load balancer</div>
              <div className="mt-1 text-xs leading-5 text-muted-foreground">
                Routes each requested model only to its owning engine.
              </div>
            </div>
            <div className="hidden items-center justify-center text-muted-foreground xl:flex">
              <ArrowRight className="size-5" />
            </div>
            <div className="grid min-w-0 gap-3 lg:grid-cols-2">
              {topology.map((engine) => {
                const serving = latest?.serving?.upstreams?.[engine.upstream]
                const metrics = latest?.engines?.[engine.upstream]
                const status = healthLabel(serving?.up)
                const color = `var(--chart-${(engine.upstream % 5) + 2})`
                return (
                  <div
                    key={`${engine.upstream}-${engine.endpoint}`}
                    className="min-w-0 rounded-xl border border-border bg-card p-4"
                    style={{ borderTopColor: color, borderTopWidth: 2 }}
                  >
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <Badge variant="outline">ENGINE {engineLetter(engine.upstream)}</Badge>
                          <Badge
                            variant="outline"
                            className={status === "serving" ? "border-emerald-500/35 text-emerald-500" : status === "unavailable" ? "border-red-500/35 text-red-500" : undefined}
                          >
                            {status}
                          </Badge>
                        </div>
                        <div className="mt-3 truncate text-base font-semibold" title={engine.model ?? "Unmapped model"}>
                          {engine.model ?? "Unmapped model"}
                        </div>
                        <div className="mt-1 truncate font-mono text-[10px] text-muted-foreground" title={engine.endpoint}>
                          {endpointLabel(engine.endpoint, engine.upstream)} · {engine.endpoint}
                        </div>
                      </div>
                      <div className="shrink-0 rounded-lg bg-muted px-3 py-2 text-center">
                        <div className="text-xl font-semibold tabular-nums">
                          {engine.tensor_parallel_size == null ? "TP—" : `TP${engine.tensor_parallel_size}`}
                        </div>
                        <div className="text-[9px] uppercase tracking-wider text-muted-foreground">tensor parallel</div>
                      </div>
                    </div>

                    <div className="mt-4 flex flex-wrap gap-1.5">
                      {engine.gpus.length ? engine.gpus.map((index) => {
                        const gpu = gpus.find((sample) => sample.index === index)
                        return (
                          <span key={index} className="inline-flex items-center gap-1.5 rounded-md bg-muted px-2 py-1 text-[11px]">
                            <Cpu className="size-3" style={{ color }} />
                            GPU {index}
                            <span className="text-muted-foreground">{fmtPct(gpu?.util_pct)}</span>
                          </span>
                        )
                      }) : (
                        <span className="text-xs text-muted-foreground">GPU ownership is not declared.</span>
                      )}
                    </div>

                    <div className="mt-4 grid grid-cols-3 gap-2 border-t border-border pt-3">
                      <div>
                        <div className="text-sm font-semibold tabular-nums">{fmtNum(serving?.inflight)}</div>
                        <div className="text-[10px] text-muted-foreground">in flight</div>
                      </div>
                      <div>
                        <div className="text-sm font-semibold tabular-nums">
                          {fmtNum(metrics?.running)} / {fmtNum(metrics?.waiting)}
                        </div>
                        <div className="text-[10px] text-muted-foreground">running / waiting</div>
                      </div>
                      <div>
                        <div className="text-sm font-semibold tabular-nums">{fmtPct(metrics?.kv_cache_pct)}</div>
                        <div className="text-[10px] text-muted-foreground">KV cache</div>
                      </div>
                    </div>
                  </div>
                )
              })}
            </div>
          </div>
        )}
        <div className="mt-3 flex items-center gap-2 text-[11px] text-muted-foreground">
          <Boxes className="size-3.5" />
          TP size is derived from each engine&apos;s configured GPU set.
        </div>
      </CardContent>
    </Card>
  )
}
