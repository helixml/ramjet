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

/**
 * Wordmark: Helix brand treatment — Sora, tight tracking, cyan→magenta
 * gradient. Body copy stays Geist; this mark is not interface text.
 */
function Wordmark() {
  return (
    <span className="flex items-center gap-2">
      <span
        aria-hidden
        className="flex size-7 shrink-0 items-center justify-center rounded-[9px] border"
        style={{
          background: "var(--wordmark-tile-bg)",
          borderColor: "var(--wordmark-tile-border)",
          boxShadow: "var(--wordmark-tile-shadow)",
        }}
      >
        <svg viewBox="0 0 24 24" className="size-[18px]">
          <defs>
            <linearGradient
              id="ramjet-mark"
              x1="2"
              y1="4"
              x2="22"
              y2="20"
              gradientUnits="userSpaceOnUse"
            >
              <stop offset="0" stopColor="#22d3ee" />
              <stop offset="1" stopColor="#e879f9" />
            </linearGradient>
          </defs>
          <g
            fill="none"
            stroke="url(#ramjet-mark)"
            strokeWidth="2.6"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M4 5.5 L10.5 12 L4 18.5" />
            <path d="M13 5.5 L19.5 12 L13 18.5" />
          </g>
        </svg>
      </span>
      <span className="wordmark">ramjet</span>
    </span>
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
        <h1>
          <Wordmark />
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
