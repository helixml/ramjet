import { useState } from "react"
import { ChartArea, Table2 } from "lucide-react"
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card"
import { Button } from "@/components/ui/button"
import { TimeChart, type BandDef, type SeriesDef, type TimeChartProps } from "@/components/TimeChart"
import { fmtClockFull } from "@/lib/format"

export interface ChartCardProps extends TimeChartProps {
  title: string
  description?: string
}

function Legend({ series, band }: { series: SeriesDef[]; band?: BandDef }) {
  // A legend is always present for ≥2 entries; one series is named by the title.
  if (series.length + (band ? 1 : 0) < 2) return null
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
      {series.map((def) => (
        <span
          key={def.key}
          className="text-muted-foreground flex items-center gap-1.5 text-[11px]"
        >
          <span
            aria-hidden
            className="h-0.5 w-3 rounded-full"
            style={{ background: def.color }}
          />
          {def.label}
        </span>
      ))}
      {band ? (
        <span className="text-muted-foreground flex items-center gap-1.5 text-[11px]">
          <span
            aria-hidden
            className="h-2 w-3 rounded-sm opacity-40"
            style={{ background: band.color }}
          />
          {band.label}
        </span>
      ) : null}
    </div>
  )
}

function TableView({
  data,
  series,
  band,
  format,
}: Pick<TimeChartProps, "data" | "series" | "band" | "format">) {
  const rows = [...data].reverse()
  const columns = [
    ...series.map((def) => ({ key: def.key, label: def.label })),
    ...(band
      ? [
          { key: band.lowKey, label: `${band.label} low` },
          { key: band.highKey, label: `${band.label} high` },
        ]
      : []),
  ]
  return (
    <div className="max-h-[180px] overflow-y-auto rounded-md border border-border">
      <table className="w-full text-[11px] tabular-nums">
        <thead className="bg-muted/60 sticky top-0">
          <tr>
            <th className="text-faint-foreground px-2 py-1 text-left font-medium">
              time
            </th>
            {columns.map((column) => (
              <th
                key={column.key}
                className="text-faint-foreground px-2 py-1 text-right font-medium"
              >
                {column.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.t} className="border-t border-border/60">
              <td className="text-muted-foreground px-2 py-0.5">
                {fmtClockFull(row.t)}
              </td>
              {columns.map((column) => {
                const value = row[column.key]
                return (
                  <td key={column.key} className="px-2 py-0.5 text-right">
                    {typeof value === "number" ? format(value) : "—"}
                  </td>
                )
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function ChartCard({ title, description, ...chart }: ChartCardProps) {
  const [table, setTable] = useState(false)
  const hasData = chart.data.some((row) =>
    chart.series.some((def) => typeof row[def.key] === "number"),
  )
  return (
    <Card>
      <CardHeader className="flex-row items-start justify-between gap-2">
        <div className="flex flex-col gap-1">
          <CardTitle>{title}</CardTitle>
          {description ? <CardDescription>{description}</CardDescription> : null}
          <Legend series={chart.series} band={chart.band} />
        </div>
        <Button
          variant="ghost"
          size="icon"
          aria-label={table ? "Show chart" : "Show table"}
          onClick={() => setTable((current) => !current)}
        >
          {table ? <ChartArea /> : <Table2 />}
        </Button>
      </CardHeader>
      <CardContent>
        {hasData ? (
          table ? (
            <TableView
              data={chart.data}
              series={chart.series}
              band={chart.band}
              format={chart.format}
            />
          ) : (
            <TimeChart {...chart} />
          )
        ) : (
          <div
            className="text-faint-foreground flex items-center justify-center text-xs"
            style={{ height: chart.height ?? 180 }}
          >
            no data in this window
          </div>
        )}
      </CardContent>
    </Card>
  )
}
