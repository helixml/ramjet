import { useEffect, useState } from "react"
import { Activity, Moon, Sun } from "lucide-react"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"

function currentTheme(): "light" | "dark" {
  const stamped = document.documentElement.dataset.theme
  if (stamped === "dark" || stamped === "light") return stamped
  return window.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light"
}

function ThemeToggle() {
  const [theme, setTheme] = useState<"light" | "dark">(currentTheme)
  useEffect(() => {
    document.documentElement.dataset.theme = theme
    try {
      localStorage.setItem("machineview-theme", theme)
    } catch {
      // Persistence is best-effort.
    }
  }, [theme])
  return (
    <Button
      variant="ghost"
      size="icon"
      aria-label={theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
      onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
    >
      {theme === "dark" ? <Sun /> : <Moon />}
    </Button>
  )
}

export function TopBar({
  hostname,
  live,
  mock,
}: {
  hostname: string | null
  live: boolean
  mock: boolean
}) {
  return (
    <header className="flex items-center justify-between gap-3">
      <div className="flex items-center gap-2.5">
        <h1 className="text-base font-semibold tracking-tight">
          mini-dynamo <span className="text-muted-foreground font-normal">machine view</span>
        </h1>
        {hostname ? <Badge variant="outline">{hostname}</Badge> : null}
        {mock ? <Badge>mock data</Badge> : null}
      </div>
      <div className="flex items-center gap-2">
        <a
          href="/metrics"
          target="_blank"
          rel="noreferrer"
          className="text-muted-foreground hover:bg-accent hover:text-accent-foreground inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-xs font-medium transition-colors"
        >
          <Activity aria-hidden className="size-3.5" />
          Prometheus
        </a>
        <span className="text-muted-foreground flex items-center gap-1.5 text-[11px]">
          <span
            aria-hidden
            className="size-1.5 rounded-full"
            style={{ background: live ? "var(--status-good)" : "var(--status-critical)" }}
          />
          {live ? "live" : "unreachable"}
        </span>
        <ThemeToggle />
      </div>
    </header>
  )
}
