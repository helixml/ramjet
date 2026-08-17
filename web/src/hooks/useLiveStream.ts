import { useEffect, useRef, useState } from "react"
import {
  isMockMode,
  streamUrl,
  type Sample,
  type ServingSample,
  type StreamFrame,
} from "@/lib/api"

/**
 * How many fast frames to hold. At the default 1 Hz this is 15 minutes,
 * which is the longest range where per-second detail is legible anyway —
 * beyond it the polled series takes over.
 */
const MAX_FRAMES = 900
const RECONNECT_MIN_MS = 1_000
const RECONNECT_MAX_MS = 15_000

export interface LiveFrame {
  t: number
  serving: ServingSample
}

export interface LiveStream {
  connected: boolean
  /** Publishing cadence the LB reported in its hello frame. */
  intervalMs: number | null
  /** Rolling window of fast serving frames, oldest first. */
  frames: LiveFrame[]
  /** Most recent full sample pushed on the slow sampling interval. */
  sample: Sample | null
}

const IDLE: LiveStream = { connected: false, intervalMs: null, frames: [], sample: null }

/**
 * Subscribes to the load balancer's live frame stream.
 *
 * The socket is an accelerator, never a requirement: polling continues
 * underneath, so a proxy that will not upgrade, an older LB without the
 * route, or a dropped connection degrades to the 5 s dashboard rather than
 * an empty one. Reconnection backs off and never gives up.
 */
export function useLiveStream(): LiveStream {
  const [state, setState] = useState<LiveStream>(IDLE)
  const mock = useRef(isMockMode()).current

  useEffect(() => {
    if (mock) return
    let socket: WebSocket | null = null
    let retry: ReturnType<typeof setTimeout> | null = null
    let backoff = RECONNECT_MIN_MS
    let closed = false

    function connect() {
      if (closed) return
      let next: WebSocket
      try {
        next = new WebSocket(streamUrl())
      } catch {
        schedule()
        return
      }
      socket = next

      next.onopen = () => {
        backoff = RECONNECT_MIN_MS
      }
      next.onmessage = (event) => {
        if (typeof event.data !== "string") return
        let frame: StreamFrame
        try {
          frame = JSON.parse(event.data) as StreamFrame
        } catch {
          return
        }
        setState((current) => {
          switch (frame.kind) {
            case "hello":
              return {
                ...current,
                connected: true,
                intervalMs: frame.stream_interval_ms,
              }
            case "serving": {
              const frames = [...current.frames, { t: frame.t, serving: frame.serving }]
              return {
                ...current,
                connected: true,
                frames: frames.length > MAX_FRAMES ? frames.slice(-MAX_FRAMES) : frames,
              }
            }
            case "sample":
              return { ...current, connected: true, sample: frame.sample }
            default:
              return current
          }
        })
      }
      next.onclose = () => {
        socket = null
        // Keep the frames already collected: the charts should not blank
        // out while the socket is coming back.
        setState((current) => ({ ...current, connected: false }))
        schedule()
      }
      next.onerror = () => next.close()
    }

    function schedule() {
      if (closed || retry != null) return
      retry = setTimeout(() => {
        retry = null
        connect()
      }, backoff)
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS)
    }

    connect()
    return () => {
      closed = true
      if (retry != null) clearTimeout(retry)
      if (socket) {
        socket.onclose = null
        socket.close()
      }
    }
  }, [mock])

  return state
}
