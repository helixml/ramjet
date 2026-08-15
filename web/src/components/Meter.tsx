import { fmtPct } from "@/lib/format"

export interface MeterProps {
  label: string
  /** 0–100 */
  pct: number | null
  detail?: string
  /** "severity" recolors the fill as it fills up; "neutral" stays accent blue. */
  tone?: "severity" | "neutral"
}

/**
 * A single ratio against a limit. The fill carries severity; the unfilled
 * track is a translucent step of the fill's own color so state reads across
 * the whole bar.
 */
export function Meter({ label, pct, detail, tone = "severity" }: MeterProps) {
  const clamped = pct == null ? null : Math.max(0, Math.min(100, pct))
  const fill =
    clamped == null
      ? "var(--axis)"
      : tone === "severity" && clamped >= 92
        ? "var(--status-critical)"
        : tone === "severity" && clamped >= 80
          ? "var(--status-warning)"
          : "var(--chart-1)"
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="text-muted-foreground truncate">{label}</span>
        <span className="shrink-0 font-medium tabular-nums">
          {fmtPct(clamped, 0)}
          {detail ? (
            <span className="text-faint-foreground ml-1.5 font-normal">{detail}</span>
          ) : null}
        </span>
      </div>
      <div
        className="h-1.5 w-full overflow-hidden rounded-full"
        style={{ background: `color-mix(in srgb, ${fill} 18%, transparent)` }}
        role="meter"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={clamped ?? undefined}
        aria-label={label}
      >
        <div
          className="h-full rounded-full"
          style={{ width: `${clamped ?? 0}%`, background: fill }}
        />
      </div>
    </div>
  )
}
