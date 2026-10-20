import { useEffect, useState } from "react"
import { cn } from "@/lib/utils"

export interface TabDef {
  id: string
  label: string
}

/** Splits `#tab/rest` into the tab id and an optional sub-route. */
function parseHash(): { tab: string; sub: string } {
  const hash = window.location.hash.slice(1)
  const slash = hash.indexOf("/")
  if (slash < 0) return { tab: hash, sub: "" }
  let sub = hash.slice(slash + 1)
  try {
    sub = decodeURIComponent(sub)
  } catch {
    // Keep the raw text for a malformed escape.
  }
  return { tab: hash.slice(0, slash), sub }
}

/**
 * Underline tabs, synced to the URL hash so a refresh keeps the view. A tab
 * may carry a sub-route (`#tokens/<id>`), which plain links can set; the
 * browser's back button then walks those like any other navigation.
 */
export function useTabs(tabs: TabDef[], fallback: string) {
  const [state, setState] = useState<{ active: string; sub: string }>(() => {
    const { tab, sub } = parseHash()
    return tabs.some((candidate) => candidate.id === tab)
      ? { active: tab, sub }
      : { active: fallback, sub: "" }
  })
  useEffect(() => {
    const onHashChange = () => {
      const { tab, sub } = parseHash()
      if (tabs.some((candidate) => candidate.id === tab)) setState({ active: tab, sub })
      else if (tab === "") setState({ active: fallback, sub: "" })
    }
    window.addEventListener("hashchange", onHashChange)
    return () => window.removeEventListener("hashchange", onHashChange)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  const select = (id: string) => {
    setState({ active: id, sub: "" })
    history.replaceState(null, "", id === fallback ? "#" : `#${id}`)
  }
  return { active: state.active, sub: state.sub, select }
}

export function TabBar({
  tabs,
  active,
  onSelect,
}: {
  tabs: TabDef[]
  active: string
  onSelect: (id: string) => void
}) {
  return (
    <div role="tablist" className="flex items-center gap-1 border-b border-border">
      {tabs.map((tab) => (
        <button
          key={tab.id}
          role="tab"
          aria-selected={tab.id === active}
          onClick={() => onSelect(tab.id)}
          className={cn(
            "-mb-px rounded-t-md border-b-2 px-3 py-1.5 text-xs font-medium transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
            tab.id === active
              ? "border-foreground text-foreground"
              : "text-muted-foreground border-transparent hover:text-foreground",
          )}
        >
          {tab.label}
        </button>
      ))}
    </div>
  )
}
