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
 * Wordmark: a compression chevron and the name, nothing else.
 *
 * The mark is the one place in this UI where color is decoration rather than
 * data, so it is kept to the chart hues already in the theme and flips with
 * them; the name itself stays in ordinary foreground ink so it reads as text
 * at any size and in forced-colors mode.
 */
function Wordmark() {
  return (
    <span className="flex items-center gap-2">
      <svg viewBox="0 0 24 24" aria-hidden className="size-[17px] shrink-0">
        <defs>
          <linearGradient
            id="ramjet-mark"
            x1="2"
            y1="4"
            x2="22"
            y2="20"
            gradientUnits="userSpaceOnUse"
          >
            <stop offset="0" stopColor="var(--chart-7)" />
            <stop offset="1" stopColor="var(--chart-1)" />
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
      <span className="text-[17px] leading-none font-semibold tracking-[-0.02em]">
        ramjet
      </span>
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
