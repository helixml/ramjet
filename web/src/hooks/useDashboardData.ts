import { useEffect, useRef, useState } from "react"
import {
  fetchSeries,
  fetchSummary,
  isMockMode,
  type Series,
  type Summary,
} from "@/lib/api"
import { mockSeries, mockSummary } from "@/lib/mock"

const SERIES_POINTS = 400

export interface DashboardData {
  summary: Summary | null
  series: Series | null
  error: string | null
  refreshing: boolean
  mock: boolean
}

/**
 * Polls summary + series on the sampling cadence. Previous data is held
 * while a refetch is in flight so charts never flash or jump.
 */
export function useDashboardData(rangeSeconds: number): DashboardData {
  const [summary, setSummary] = useState<Summary | null>(null)
  const [series, setSeries] = useState<Series | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [refreshing, setRefreshing] = useState(false)
  const mock = useRef(isMockMode()).current
  const generation = useRef(0)

  useEffect(() => {
    let cancelled = false
    const ticket = ++generation.current

    async function load() {
      if (cancelled || ticket !== generation.current) return
      setRefreshing(true)
      try {
        const [nextSummary, nextSeries] = mock
          ? [mockSummary(), mockSeries(rangeSeconds, SERIES_POINTS)]
          : await Promise.all([
              fetchSummary(),
              fetchSeries(rangeSeconds, SERIES_POINTS),
            ])
        if (cancelled || ticket !== generation.current) return
        setSummary(nextSummary)
        setSeries(nextSeries)
        setError(null)
      } catch (cause) {
        if (cancelled || ticket !== generation.current) return
        setError(cause instanceof Error ? cause.message : String(cause))
      } finally {
        if (!cancelled && ticket === generation.current) setRefreshing(false)
      }
    }

    void load()
    const interval = setInterval(() => void load(), 5000)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [rangeSeconds, mock])

  return { summary, series, error, refreshing, mock }
}
