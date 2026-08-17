import { useRef, useState } from "react"
import { ChartArea, Table2 } from "lucide-react"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"

/** The five sequential steps, quietest first. Empty cells use none of them. */
const RAMP = ["var(--seq-1)", "var(--seq-2)", "var(--seq-3)", "var(--seq-4)", "var(--seq-5)"]
/** Footprint of the idle speck and the unknown ring, as % of the cell. */
const QUIET_DOT_PCT = 22

export interface HeatmapMetric {
  label: string
  value: string
}

export interface HeatmapCell {
  row: number
  column: number
  /** Human label for the tooltip and the table view, e.g. "Tue 12 Aug". */
  label: string
  /** `null` means nothing was recorded for this cell — not "zero". */
  value: number | null
  metrics?: HeatmapMetric[]
}

export interface HeatmapScale {
  /** 0 = empty/idle, 1..5 = ramp step. */
  step: (value: number | null) => number
  /** Dot area relative to the cell, 0..1 — area is proportional to value. */
  area: (value: number | null) => number
  thresholds: number[]
}

/**
 * Two encodings of the same number, because neither alone reads well here.
 *
 * **Size** is area-proportional to the value, so one enormous hour looks
 * enormous — the honest encoding, and the one that makes the shape jump out.
 * **Color** is bucketed by quantile of the *active* cells, because token
 * volume is skewed enough (a busy hour can be a hundred times a quiet one)
 * that the area encoding alone leaves the whole quiet half as indistinct
 * specks. Exact values stay one hover, or the table view, away.
 */
export function buildScale(values: Array<number | null>): HeatmapScale {
  const active = values
    .filter((value): value is number => typeof value === "number" && value > 0)
    .sort((a, b) => a - b)
  const thresholds = active.length
    ? [0.2, 0.4, 0.6, 0.8].map(
        (quantile) => active[Math.min(active.length - 1, Math.floor(quantile * active.length))],
      )
    : []
  const peak = active[active.length - 1] ?? 0
  return {
    thresholds,
    step: (value) => {
      if (value == null || value <= 0) return 0
      let step = 1
      for (const threshold of thresholds) {
        if (value > threshold) step += 1
      }
      return Math.min(step, RAMP.length)
    },
    // A floor keeps the quietest active cell visible as a dot rather than a
    // sub-pixel; idle and unknown cells are drawn by their own rules.
    area: (value) => {
      if (value == null || value <= 0 || peak <= 0) return 0
      return Math.max(0.06, Math.min(1, value / peak))
    },
  }
}

interface HeatmapProps {
  rowLabels: string[]
  /** One entry per column; `null` renders no label (sparse axis). */
  columnLabels: Array<string | null>
  cells: HeatmapCell[]
  format: (value: number) => string
  /** Unit suffix for the tooltip and screen readers; the table has a header. */
  unit: string
  scale: HeatmapScale
  /** Announced description of what one cell covers, for screen readers. */
  cellRole: string
  /**
   * Cells stay square, so an unbounded grid in a wide card produces
   * enormous tiles and a card taller than the screen. Few columns simply
   * means a small grid.
   */
  cellMaxPx?: number
  /** Where the capped grid sits when it is narrower than the card. */
  align?: "start" | "center"
  /** Stand long column labels on end so 30 dates fit without colliding. */
  rotateColumnLabels?: boolean
}

function Heatmap({
  rowLabels,
  columnLabels,
  cells,
  format,
  unit,
  scale,
  cellRole,
  cellMaxPx = 26,
  align = "start",
  rotateColumnLabels = false,
}: HeatmapProps) {
  const container = useRef<HTMLDivElement>(null)
  const [active, setActive] = useState<{
    cell: HeatmapCell
    left: number
    top: number
    below: boolean
  } | null>(null)

  const byPosition = new Map(cells.map((cell) => [`${cell.row}:${cell.column}`, cell]))

  function show(cell: HeatmapCell, element: HTMLElement) {
    const width = container.current?.clientWidth ?? 0
    const left = element.offsetLeft + element.offsetWidth / 2
    // Top rows have no room above them, so the bubble drops below instead of
    // being clipped by the card.
    const below = element.offsetTop < 90
    setActive({
      cell,
      // Keep the bubble inside the card at the first and last column.
      left: Math.min(Math.max(left, 74), Math.max(width - 74, 74)),
      top: below ? element.offsetTop + element.offsetHeight + 6 : element.offsetTop - 6,
      below,
    })
  }

  return (
    <div ref={container} className="relative">
      <div
        role="grid"
        className={`grid gap-[2px] ${align === "center" ? "justify-center" : "justify-start"}`}
        style={{
          gridTemplateColumns: `auto repeat(${columnLabels.length}, minmax(0, ${cellMaxPx}px))`,
        }}
      >
        <div aria-hidden />
        {columnLabels.map((label, column) => (
          <div
            key={`head-${column}`}
            aria-hidden
            className={
              rotateColumnLabels
                ? "text-faint-foreground flex h-11 items-end justify-center pb-1 text-[10px] whitespace-nowrap"
                : "text-faint-foreground overflow-visible pb-1 text-[10px] whitespace-nowrap"
            }
            style={
              rotateColumnLabels
                ? { writingMode: "vertical-rl", transform: "rotate(180deg)" }
                : undefined
            }
          >
            {label ?? ""}
          </div>
        ))}
        {rowLabels.map((rowLabel, row) => (
          <div key={`row-${row}`} className="contents" role="row">
            <div
              className="text-faint-foreground flex items-center pr-2 text-[10px] whitespace-nowrap"
            >
              {rowLabel}
            </div>
            {columnLabels.map((_, column) => {
              const cell = byPosition.get(`${row}:${column}`)
              const step = scale.step(cell?.value ?? null)
              // Three states, and the distinction matters: an hour the box
              // was idle is a measurement, an hour with no bucket at all
              // (the LB was down, or history starts later) is not.
              const unknown = cell == null || cell.value == null
              const idle = !unknown && step === 0
              return (
                <div
                  key={`cell-${row}-${column}`}
                  role="gridcell"
                  tabIndex={cell ? 0 : undefined}
                  aria-label={
                    cell
                      ? `${cell.label}: ${
                          cell.value == null ? "no data" : `${format(cell.value)} ${unit}`
                        } ${cellRole}`
                      : undefined
                  }
                  aria-hidden={cell ? undefined : true}
                  className="flex aspect-square min-h-[7px] w-full items-center justify-center rounded-[3px] outline-offset-1 focus-visible:outline-2 focus-visible:outline-[var(--ring)]"
                  onMouseEnter={(event) => cell && show(cell, event.currentTarget)}
                  onFocus={(event) => cell && show(cell, event.currentTarget)}
                  onMouseLeave={() => setActive(null)}
                  onBlur={() => setActive(null)}
                >
                  {/* Area ∝ value, so the side is its square root. Idle is a
                      solid speck and unknown a hollow ring: same footprint,
                      unmistakably different marks. */}
                  <span
                    aria-hidden
                    className="pointer-events-none block rounded-full"
                    style={
                      unknown || idle
                        ? {
                            width: `${QUIET_DOT_PCT}%`,
                            height: `${QUIET_DOT_PCT}%`,
                            background: idle ? "var(--muted-foreground)" : "transparent",
                            opacity: idle ? 0.45 : 1,
                            boxShadow: unknown ? "inset 0 0 0 1px var(--grid)" : undefined,
                          }
                        : {
                            width: `${Math.sqrt(scale.area(cell?.value ?? null)) * 100}%`,
                            height: `${Math.sqrt(scale.area(cell?.value ?? null)) * 100}%`,
                            background: RAMP[step - 1],
                          }
                    }
                  />
                </div>
              )
            })}
          </div>
        ))}
      </div>
      {active ? (
        <div
          role="tooltip"
          className={`bg-card pointer-events-none absolute z-10 -translate-x-1/2 rounded-md border border-border px-2 py-1.5 text-[11px] shadow-md ${
            active.below ? "" : "-translate-y-full"
          }`}
          style={{ left: active.left, top: active.top }}
        >
          <div className="text-muted-foreground whitespace-nowrap">{active.cell.label}</div>
          <div className="text-foreground tabular-nums whitespace-nowrap">
            {active.cell.value == null
              ? "no data"
              : `${format(active.cell.value)} ${unit}`}
          </div>
          {active.cell.metrics?.map((metric) => (
            <div
              key={metric.label}
              className="text-muted-foreground flex justify-between gap-3 tabular-nums whitespace-nowrap"
            >
              <span>{metric.label}</span>
              <span>{metric.value}</span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

function ScaleLegend() {
  return (
    <div className="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
      <span className="flex items-center gap-1.5">
        idle
        <span
          aria-hidden
          className="h-1 w-1 rounded-full"
          style={{ background: "var(--muted-foreground)", opacity: 0.45 }}
        />
      </span>
      <span className="flex items-center gap-1.5">
        quiet
        {RAMP.map((color, index) => {
          const size = 3 + index * 2.5
          return (
            <span
              key={color}
              aria-hidden
              className="rounded-full"
              style={{ background: color, width: size, height: size }}
            />
          )
        })}
        busy
      </span>
      <span className="flex items-center gap-1.5">
        <span
          aria-hidden
          className="h-1.5 w-1.5 rounded-full"
          style={{ boxShadow: "inset 0 0 0 1px var(--grid)" }}
        />
        no data
      </span>
    </div>
  )
}

function TableView({
  cells,
  format,
}: {
  cells: HeatmapCell[]
  format: (value: number) => string
}) {
  const rows = cells
    .filter((cell) => cell.value != null)
    .sort((a, b) => (b.value ?? 0) - (a.value ?? 0))
  const metrics = rows.find((row) => row.metrics?.length)?.metrics ?? []
  return (
    <div className="max-h-[220px] overflow-y-auto rounded-md border border-border">
      <table className="w-full text-[11px] tabular-nums">
        <thead className="bg-muted/60 sticky top-0">
          <tr>
            <th className="text-faint-foreground px-2 py-1 text-left font-medium">when</th>
            <th className="text-faint-foreground px-2 py-1 text-right font-medium">tokens</th>
            {metrics.map((metric) => (
              <th
                key={metric.label}
                className="text-faint-foreground px-2 py-1 text-right font-medium"
              >
                {metric.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={`${row.row}:${row.column}`} className="border-t border-border/60">
              <td className="text-muted-foreground px-2 py-0.5">{row.label}</td>
              <td className="px-2 py-0.5 text-right">{format(row.value ?? 0)}</td>
              {metrics.map((metric) => (
                <td key={metric.label} className="px-2 py-0.5 text-right">
                  {row.metrics?.find((entry) => entry.label === metric.label)?.value ?? "—"}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export interface HeatmapCardProps extends HeatmapProps {
  title: string
  description?: string
  loading?: boolean
}

/** A magnitude grid with the same chart/table/legend contract as ChartCard. */
export function HeatmapCard({
  title,
  description,
  loading = false,
  ...heatmap
}: HeatmapCardProps) {
  const [table, setTable] = useState(false)
  const hasData = heatmap.cells.some((cell) => cell.value != null)
  return (
    <Card aria-busy={loading || undefined}>
      <CardHeader className="flex-row items-start justify-between gap-2">
        <div className="flex flex-col gap-1">
          <CardTitle>{title}</CardTitle>
          {loading ? (
            <Skeleton className="h-3 w-40" />
          ) : description ? (
            <CardDescription>{description}</CardDescription>
          ) : null}
          {loading ? null : <ScaleLegend />}
        </div>
        <Button
          variant="ghost"
          size="icon"
          aria-label={table ? "Show chart" : "Show table"}
          onClick={() => setTable((current) => !current)}
          disabled={loading}
        >
          {table ? <ChartArea /> : <Table2 />}
        </Button>
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="h-[150px] w-full rounded-md" />
        ) : hasData ? (
          table ? (
            <TableView cells={heatmap.cells} format={heatmap.format} />
          ) : (
            <Heatmap {...heatmap} />
          )
        ) : (
          <div className="text-faint-foreground flex h-[150px] items-center justify-center text-center text-xs">
            no token history yet — the load balancer records one bucket an hour
          </div>
        )}
      </CardContent>
    </Card>
  )
}
