import { useEffect, useRef } from "react"
import { createPortal } from "react-dom"
import {
  AlertTriangle,
  ArrowRight,
  Check,
  Clock3,
  ServerCog,
  ShieldCheck,
  X,
} from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import type { AdaptiveProfile, AdaptiveTransition } from "@/lib/api"

function gpuOwner(profile: AdaptiveProfile, gpu: number): number {
  return Math.max(
    0,
    profile.engines.findIndex((engine) => engine.gpus.includes(gpu)),
  )
}

function ShapePreview({ profile, active }: { profile: AdaptiveProfile; active?: boolean }) {
  return (
    <div className={`min-w-0 flex-1 rounded-xl border p-3 ${active ? "border-border bg-muted/45" : "border-primary/30 bg-primary/5"}`}>
      <div className="flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="truncate text-sm font-semibold">{profile.label}</div>
          <div className="mt-0.5 text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
            {active ? "Current shape" : "Target shape"}
          </div>
        </div>
        <Badge variant="outline">{profile.engines.length === 1 ? "TP8" : "2 × TP4"}</Badge>
      </div>
      <div className="mt-3 grid grid-cols-4 gap-1.5">
        {Array.from({ length: 8 }, (_, gpu) => {
          const owner = gpuOwner(profile, gpu)
          const color = owner === 0 ? "#00d5ff" : "#a855f7"
          return (
            <div
              key={gpu}
              className="relative flex h-8 items-center justify-center overflow-hidden rounded-md border text-[9px] font-medium"
              style={{
                borderColor: `color-mix(in srgb, ${color} 45%, transparent)`,
                background: `color-mix(in srgb, ${color} 9%, var(--card))`,
              }}
            >
              <span className="relative z-10">G{gpu}</span>
              <span className="absolute inset-x-0 bottom-0 h-0.5" style={{ background: color }} />
            </div>
          )
        })}
      </div>
      <div className="mt-2 flex flex-wrap gap-1">
        {profile.engines.map((engine) => (
          <span key={engine.container} className="rounded bg-card px-1.5 py-0.5 font-mono text-[9px] text-muted-foreground">
            {engine.label}
          </span>
        ))}
      </div>
    </div>
  )
}

export function TransitionDialog({
  current,
  target,
  transition,
  working,
  error,
  onCancel,
  onConfirm,
}: {
  current: AdaptiveProfile
  target: AdaptiveProfile
  transition: AdaptiveTransition
  working: boolean
  error: string | null
  onCancel: () => void
  onConfirm: () => void
}) {
  const dialog = useRef<HTMLDivElement>(null)
  const cancel = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    const previousOverflow = document.body.style.overflow
    document.body.style.overflow = "hidden"
    cancel.current?.focus()
    const keydown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !working) onCancel()
      if (event.key !== "Tab" || !dialog.current) return
      const focusable = [...dialog.current.querySelectorAll<HTMLElement>("button:not(:disabled)")]
      if (focusable.length === 0) return
      const first = focusable[0]
      const last = focusable.at(-1) ?? first
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault()
        first.focus()
      }
    }
    document.addEventListener("keydown", keydown)
    return () => {
      document.body.style.overflow = previousOverflow
      document.removeEventListener("keydown", keydown)
    }
  }, [onCancel, working])

  const minutes = Math.max(1, Math.round(transition.estimated_downtime_seconds / 60))

  return createPortal(
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-black/75 p-4 backdrop-blur-md"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !working) onCancel()
      }}
    >
      <div
        ref={dialog}
        role="dialog"
        aria-modal="true"
        aria-labelledby="transition-title"
        aria-describedby="transition-description"
        className="relative w-full max-w-[620px] overflow-hidden rounded-2xl border border-border bg-card shadow-[0_28px_100px_rgb(0_0_0/0.65)]"
      >
        <div aria-hidden className="pointer-events-none absolute inset-x-0 top-0 h-32 bg-[radial-gradient(ellipse_at_top,rgba(0,213,255,0.12),transparent_70%)]" />

        <div className="relative flex items-start justify-between gap-4 px-5 pb-4 pt-5 md:px-6 md:pt-6">
          <div className="flex gap-3">
            <span className="flex size-10 shrink-0 items-center justify-center rounded-xl border border-primary/20 bg-primary/10 text-primary">
              <ServerCog className="size-5" />
            </span>
            <div>
              <h2 id="transition-title" className="text-lg font-semibold tracking-tight">Reconfigure engine topology?</h2>
              <p id="transition-description" className="mt-1 text-xs leading-5 text-muted-foreground">
                Ramjet will drain the current shape, reassign all eight GPUs, and qualify the new engines before serving resumes.
              </p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Close dialog"
            disabled={working}
            onClick={onCancel}
            className="flex size-8 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition hover:bg-muted hover:text-foreground disabled:opacity-40"
          >
            <X className="size-4" />
          </button>
        </div>

        <div className="relative px-5 pb-5 md:px-6">
          <div className="flex items-stretch gap-2">
            <ShapePreview profile={current} active />
            <div className="flex shrink-0 items-center text-muted-foreground"><ArrowRight className="size-5" /></div>
            <ShapePreview profile={target} />
          </div>

          {transition.requires_downtime ? (
            <div className="mt-4 flex gap-3 rounded-xl border border-amber-500/25 bg-amber-500/[0.07] p-3.5">
              <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-500" />
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs font-semibold">
                  Serving interruption
                  <span className="inline-flex items-center gap-1 font-mono text-amber-500"><Clock3 className="size-3" /> about {minutes} min</span>
                </div>
                <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
                  New inference requests will fail fast while the target model loads. The dashboard stays available throughout.
                </p>
              </div>
            </div>
          ) : (
            <div className="mt-4 flex items-center gap-2 rounded-xl border border-emerald-500/25 bg-emerald-500/[0.07] p-3 text-xs">
              <ShieldCheck className="size-4 text-emerald-500" /> Serving remains available during this change.
            </div>
          )}

          <div className="mt-4 grid grid-cols-2 gap-2 md:grid-cols-4">
            {["Fence & drain", "Stop current", "Start & warm", "Qualify & admit"].map((step, index) => (
              <div key={step} className="rounded-lg border border-border bg-muted/35 px-2.5 py-2">
                <div className="mb-1 flex items-center gap-1.5 text-[9px] uppercase tracking-[0.14em] text-muted-foreground">
                  <span className="flex size-4 items-center justify-center rounded-full bg-primary/10 text-[9px] text-primary">{index + 1}</span>
                  Stage
                </div>
                <div className="text-[11px] font-medium">{step}</div>
              </div>
            ))}
          </div>

          {error ? (
            <div className="mt-4 rounded-lg border border-red-500/30 bg-red-500/5 px-3 py-2 text-xs text-red-500">{error}</div>
          ) : null}
        </div>

        <div className="flex flex-col-reverse gap-2 border-t border-border bg-muted/25 px-5 py-4 sm:flex-row sm:items-center sm:justify-between md:px-6">
          <span className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
            <Check className="size-3 text-primary" /> Named container authority verified at startup
          </span>
          <div className="flex justify-end gap-2">
            <Button ref={cancel} variant="outline" disabled={working} onClick={onCancel}>Cancel</Button>
            <Button disabled={working} onClick={onConfirm} className="min-w-40">
              {working ? "Starting transition…" : "Begin reconfiguration"}
              {!working ? <ArrowRight /> : null}
            </Button>
          </div>
        </div>
      </div>
    </div>,
    document.body,
  )
}
