import { Button } from "@/components/ui/button"

export interface RangeOption {
  label: string
  seconds: number
}

export const RANGE_OPTIONS: RangeOption[] = [
  { label: "30m", seconds: 30 * 60 },
  { label: "1h", seconds: 3600 },
  { label: "3h", seconds: 3 * 3600 },
  { label: "6h", seconds: 6 * 3600 },
  { label: "12h", seconds: 12 * 3600 },
  { label: "24h", seconds: 24 * 3600 },
]

export function RangePicker({
  value,
  onChange,
}: {
  value: number
  onChange: (seconds: number) => void
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Time range"
      className="bg-muted flex items-center gap-0.5 rounded-lg p-0.5"
    >
      {RANGE_OPTIONS.map((option) => (
        <Button
          key={option.seconds}
          variant="segment"
          role="radio"
          aria-checked={option.seconds === value}
          data-active={option.seconds === value}
          onClick={() => onChange(option.seconds)}
        >
          {option.label}
        </Button>
      ))}
    </div>
  )
}
