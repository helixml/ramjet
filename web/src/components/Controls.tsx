import { useEffect, useRef, type ReactNode } from "react"
import { createPortal } from "react-dom"
import { X } from "lucide-react"
import { Button } from "@/components/ui/button"
import { cn } from "@/lib/utils"

/** Segmented single-choice control, styled like the range picker. */
export function Segmented<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string
  value: T
  options: Array<{ value: T; label: string }>
  onChange: (value: T) => void
}) {
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="bg-muted flex items-center gap-0.5 rounded-lg p-0.5"
    >
      {options.map((option) => (
        <Button
          key={option.value}
          variant="segment"
          role="radio"
          aria-checked={option.value === value}
          data-active={option.value === value}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </Button>
      ))}
    </div>
  )
}

/** Compact labelled native select. */
export function SelectField({
  label,
  value,
  options,
  onChange,
  className,
}: {
  label: string
  value: string
  options: Array<{ value: string; label: string }>
  onChange: (value: string) => void
  className?: string
}) {
  return (
    <label className={cn("flex items-center gap-1.5 text-[11px] text-muted-foreground", className)}>
      {label}
      <select
        value={value}
        onChange={(event) => onChange(event.target.value)}
        className="h-7 max-w-[200px] rounded-md border border-border bg-card px-2 text-xs text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </label>
  )
}

export const inputClass =
  "w-full rounded-md border border-border bg-card px-2.5 py-1.5 text-xs text-foreground outline-none placeholder:text-faint-foreground focus-visible:ring-2 focus-visible:ring-ring/50 disabled:opacity-50"

/** Modal shell: portal, backdrop, Escape to close, focus trap. */
export function Modal({
  title,
  description,
  icon,
  busy = false,
  onClose,
  children,
  footer,
  width = 520,
}: {
  title: string
  description?: ReactNode
  icon?: ReactNode
  busy?: boolean
  onClose: () => void
  children?: ReactNode
  footer: ReactNode
  width?: number
}) {
  const dialog = useRef<HTMLDivElement>(null)
  // Callers pass inline closures; keep the effect mount-only so a re-render
  // never steals focus from the field being typed in.
  const closeRef = useRef(onClose)
  closeRef.current = onClose
  const busyRef = useRef(busy)
  busyRef.current = busy

  useEffect(() => {
    const previousOverflow = document.body.style.overflow
    document.body.style.overflow = "hidden"
    const first = dialog.current?.querySelector<HTMLElement>(
      "input, textarea, select, [data-autofocus]",
    )
    first?.focus()
    const keydown = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !busyRef.current) closeRef.current()
      if (event.key !== "Tab" || !dialog.current) return
      const focusable = [
        ...dialog.current.querySelectorAll<HTMLElement>(
          "button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled)",
        ),
      ]
      if (focusable.length === 0) return
      const head = focusable[0]
      const tail = focusable.at(-1) ?? head
      if (event.shiftKey && document.activeElement === head) {
        event.preventDefault()
        tail.focus()
      } else if (!event.shiftKey && document.activeElement === tail) {
        event.preventDefault()
        head.focus()
      }
    }
    document.addEventListener("keydown", keydown)
    return () => {
      document.body.style.overflow = previousOverflow
      document.removeEventListener("keydown", keydown)
    }
  }, [])

  return createPortal(
    <div
      className="fixed inset-0 z-[100] flex items-center justify-center bg-black/70 p-4 backdrop-blur-sm"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && !busy) onClose()
      }}
    >
      <div
        ref={dialog}
        role="dialog"
        aria-modal="true"
        aria-label={title}
        className="relative w-full overflow-hidden rounded-xl border border-border bg-card shadow-[0_28px_100px_rgb(0_0_0/0.5)]"
        style={{ maxWidth: width }}
      >
        <div className="flex items-start justify-between gap-4 px-5 pt-5 pb-3">
          <div className="flex gap-3">
            {icon ? (
              <span className="flex size-9 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-foreground">
                {icon}
              </span>
            ) : null}
            <div>
              <h2 className="text-base font-semibold tracking-tight">{title}</h2>
              {description ? (
                <div className="mt-1 text-xs leading-5 text-muted-foreground">{description}</div>
              ) : null}
            </div>
          </div>
          <button
            type="button"
            aria-label="Close dialog"
            disabled={busy}
            onClick={onClose}
            className="flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground transition hover:bg-muted hover:text-foreground disabled:opacity-40"
          >
            <X className="size-4" />
          </button>
        </div>
        {children ? <div className="px-5 pb-4">{children}</div> : null}
        <div className="flex justify-end gap-2 border-t border-border bg-muted/25 px-5 py-3">
          {footer}
        </div>
      </div>
    </div>,
    document.body,
  )
}
