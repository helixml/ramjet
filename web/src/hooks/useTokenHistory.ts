import { useEffect, useRef, useState } from "react"
import { fetchTokens, isMockMode, type TokenHistory } from "@/lib/api"
import { mockTokens } from "@/lib/mock"

// The history advances one bucket an hour, so polling it on the 5 s
// dashboard cadence would be 720 wasted requests a day.
const REFRESH_MS = 60_000

export interface TokenHistoryData {
  tokens: TokenHistory | null
  error: string | null
  mock: boolean
}

/** Polls the hourly token history used by the two magnitude heatmaps. */
export function useTokenHistory(days: number): TokenHistoryData {
  const [tokens, setTokens] = useState<TokenHistory | null>(null)
  const [error, setError] = useState<string | null>(null)
  const mock = useRef(isMockMode()).current
  const generation = useRef(0)

  useEffect(() => {
    let cancelled = false
    const ticket = ++generation.current

    async function load() {
      if (cancelled || ticket !== generation.current) return
      try {
        const next = mock ? mockTokens(days) : await fetchTokens(days)
        if (cancelled || ticket !== generation.current) return
        setTokens(next)
        setError(null)
      } catch (cause) {
        if (cancelled || ticket !== generation.current) return
        setError(cause instanceof Error ? cause.message : String(cause))
      }
    }

    void load()
    const interval = setInterval(() => void load(), REFRESH_MS)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [days, mock])

  return { tokens, error, mock }
}
