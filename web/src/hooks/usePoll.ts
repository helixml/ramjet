import { useCallback, useEffect, useRef, useState } from "react"
import { UnauthorizedError } from "@/lib/accounting"

export interface Poll<T> {
  data: T | null
  error: string | null
  /** Set once the route answered 404: the feature is absent on this LB. */
  missing: boolean
  loading: boolean
  refresh: () => void
}

/**
 * Fetch on mount / key change and every `intervalMs` (0 = never). Previous
 * data is held across refetches so tables and charts never flash. A 401
 * hands control back to the app's login screen.
 */
export function usePoll<T>(
  load: () => Promise<T>,
  key: string,
  intervalMs: number,
  onUnauthorized?: () => void,
): Poll<T> {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [missing, setMissing] = useState(false)
  const [loading, setLoading] = useState(true)
  const [tick, setTick] = useState(0)
  const loadRef = useRef(load)
  loadRef.current = load
  const unauthorizedRef = useRef(onUnauthorized)
  unauthorizedRef.current = onUnauthorized
  const generation = useRef(0)
  const lastKey = useRef(key)

  useEffect(() => {
    let cancelled = false
    const ticket = ++generation.current
    if (lastKey.current !== key) {
      // A different query: drop the stale answer rather than mislabel it.
      lastKey.current = key
      setData(null)
      setLoading(true)
    }
    async function run() {
      try {
        const next = await loadRef.current()
        if (cancelled || ticket !== generation.current) return
        setData(next)
        setError(null)
        setMissing(false)
      } catch (cause) {
        if (cancelled || ticket !== generation.current) return
        if (cause instanceof UnauthorizedError) {
          unauthorizedRef.current?.()
          return
        }
        if (cause instanceof Error && cause.name === "NotFoundError") setMissing(true)
        setError(cause instanceof Error ? cause.message : String(cause))
      } finally {
        if (!cancelled && ticket === generation.current) setLoading(false)
      }
    }
    void run()
    if (intervalMs <= 0) {
      return () => {
        cancelled = true
      }
    }
    const interval = setInterval(() => {
      if (document.visibilityState === "hidden") return
      void run()
    }, intervalMs)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [key, intervalMs, tick])

  const refresh = useCallback(() => setTick((value) => value + 1), [])
  return { data, error, missing, loading, refresh }
}
