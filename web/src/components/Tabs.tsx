import { useEffect, useState } from "react"
import { cn } from "@/lib/utils"

export interface TabDef {
  id: string
  label: string
}

/** Underline tabs, synced to the URL hash so a refresh keeps the view. */
export function useTabs(tabs: TabDef[], fallback: string) {
  const [active, setActive] = useState<string>(() => {
    const hash = window.location.hash.slice(1)
    return tabs.some((tab) => tab.id === hash) ? hash : fallback
  })
  useEffect(() => {
    const onHashChange = () => {
      const hash = window.location.hash.slice(1)
      if (tabs.some((tab) => tab.id === hash)) setActive(hash)
    }
    window.addEventListener("hashchange", onHashChange)
    return () => window.removeEventListener("hashchange", onHashChange)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
  const select = (id: string) => {
    setActive(id)
    history.replaceState(null, "", id === fallback ? "#" : `#${id}`)
  }
  return { active, select }
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
