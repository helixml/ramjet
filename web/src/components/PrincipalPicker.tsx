import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from "react"
import { ChevronsUpDown, X } from "lucide-react"
import type { Principal } from "@/lib/accounting"
import { cn } from "@/lib/utils"

/** Rows rendered in the open list; the search narrows anything beyond. */
const MAX_SHOWN = 60

function describe(principal: Principal): string {
  if (principal.kind === "token") {
    return principal.status && principal.status !== "active" ? principal.status : "token"
  }
  return principal.kind.replace("_", " ")
}

/**
 * Searchable principal combobox. A native <select> with ten thousand
 * options is unusable; this filters by name, id, or kind as you type and
 * renders at most a screenful of matches.
 */
export function PrincipalPicker({
  principals,
  value,
  onChange,
  label = "token",
  allLabel = "all principals",
}: {
  principals: Principal[]
  value: string
  onChange: (key: string) => void
  label?: string
  allLabel?: string
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState("")
  const [active, setActive] = useState(0)
  const root = useRef<HTMLDivElement>(null)
  const input = useRef<HTMLInputElement>(null)
  const listId = useId()

  const selected = principals.find((principal) => principal.key === value)
  const matches = useMemo(() => {
    const needle = query.trim().toLowerCase()
    const hits = needle
      ? principals.filter(
          (principal) =>
            principal.name.toLowerCase().includes(needle) ||
            principal.key.toLowerCase().includes(needle) ||
            principal.kind.includes(needle),
        )
      : principals
    return { total: hits.length, shown: hits.slice(0, MAX_SHOWN) }
  }, [principals, query])
  // Index 0 is always "all principals".
  const options = useMemo(
    () => [null, ...matches.shown] as Array<Principal | null>,
    [matches],
  )

  useEffect(() => {
    if (!open) return
    const close = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", close)
    return () => document.removeEventListener("mousedown", close)
  }, [open])

  const choose = (principal: Principal | null) => {
    onChange(principal?.key ?? "")
    setOpen(false)
    setQuery("")
  }

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault()
      setOpen(true)
      setActive((index) => Math.min(options.length - 1, index + 1))
    } else if (event.key === "ArrowUp") {
      event.preventDefault()
      setActive((index) => Math.max(0, index - 1))
    } else if (event.key === "Enter") {
      event.preventDefault()
      if (open) choose(options[active] ?? null)
      else setOpen(true)
    } else if (event.key === "Escape") {
      setOpen(false)
      setQuery("")
    }
  }

  return (
    <div ref={root} className="relative flex items-center gap-1.5 text-[11px] text-muted-foreground">
      <span>{label}</span>
      <div className="relative">
        <input
          ref={input}
          role="combobox"
          aria-expanded={open}
          aria-controls={listId}
          aria-label={label}
          aria-activedescendant={open ? `${listId}-${active}` : undefined}
          value={open ? query : (selected?.name ?? (value || allLabel))}
          placeholder={selected?.name ?? allLabel}
          onFocus={() => {
            setOpen(true)
            setActive(0)
          }}
          onClick={() => setOpen(true)}
          onChange={(event) => {
            setQuery(event.target.value)
            setActive(0)
            setOpen(true)
          }}
          onKeyDown={onKeyDown}
          className={cn(
            "h-7 w-[200px] rounded-md border border-border bg-card pr-12 pl-2 text-xs text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
            !open && !value && "text-muted-foreground",
          )}
        />
        <span className="pointer-events-none absolute top-1/2 right-1.5 -translate-y-1/2">
          <ChevronsUpDown className="size-3.5" />
        </span>
        {value ? (
          <button
            type="button"
            aria-label="Clear principal filter"
            onClick={() => choose(null)}
            className="absolute top-1/2 right-6 -translate-y-1/2 rounded p-0.5 hover:bg-muted hover:text-foreground"
          >
            <X className="size-3" />
          </button>
        ) : null}
      </div>
      {open ? (
        <ul
          id={listId}
          role="listbox"
          className="absolute top-8 left-0 z-50 max-h-[320px] w-[320px] overflow-y-auto rounded-md border border-border bg-card py-1 shadow-lg"
        >
          {options.map((principal, index) => (
            <li
              key={principal?.key ?? "__all"}
              id={`${listId}-${index}`}
              role="option"
              aria-selected={(principal?.key ?? "") === value}
              onMouseDown={(event) => {
                event.preventDefault()
                choose(principal)
              }}
              onMouseEnter={() => setActive(index)}
              className={cn(
                "flex cursor-pointer items-center justify-between gap-2 px-2.5 py-1 text-xs",
                index === active && "bg-muted",
                (principal?.key ?? "") === value && "font-medium",
              )}
            >
              <span className="truncate text-foreground">{principal ? principal.name : allLabel}</span>
              {principal ? (
                <span className="text-faint-foreground shrink-0 font-mono text-[10px]">
                  {describe(principal)}
                </span>
              ) : null}
            </li>
          ))}
          {matches.total > matches.shown.length ? (
            <li className="text-faint-foreground px-2.5 py-1 text-[11px]">
              {(matches.total - matches.shown.length).toLocaleString("en-US")} more — keep typing to narrow
            </li>
          ) : null}
          {matches.total === 0 ? (
            <li className="text-faint-foreground px-2.5 py-1 text-[11px]">no principal matches “{query}”</li>
          ) : null}
        </ul>
      ) : null}
    </div>
  )
}
