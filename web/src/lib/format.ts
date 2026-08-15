const BYTE_UNITS = ["B", "KiB", "MiB", "GiB", "TiB"]

export function fmtBytes(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—"
  let scaled = value
  let unit = 0
  while (scaled >= 1024 && unit < BYTE_UNITS.length - 1) {
    scaled /= 1024
    unit += 1
  }
  const digits = scaled >= 100 ? 0 : scaled >= 10 ? 1 : scaled > 0 ? 1 : 0
  return `${scaled.toFixed(digits)} ${BYTE_UNITS[unit]}`
}

export function fmtBps(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—"
  return `${fmtBytes(value)}/s`
}

export function fmtNum(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return "—"
  if (Math.abs(value) >= 10_000) {
    return `${(value / 1000).toFixed(1)}K`
  }
  return value.toLocaleString("en-US", {
    maximumFractionDigits: digits,
    minimumFractionDigits: 0,
  })
}

export function fmtPct(value: number | null | undefined, digits = 0): string {
  if (value == null || !Number.isFinite(value)) return "—"
  return `${value.toFixed(digits)}%`
}

export function fmtMs(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—"
  if (value >= 10_000) return `${(value / 1000).toFixed(1)} s`
  if (value >= 1_000) return `${(value / 1000).toFixed(2)} s`
  return `${value.toFixed(0)} ms`
}

export function fmtWatts(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—"
  if (value >= 1000) return `${(value / 1000).toFixed(2)} kW`
  return `${value.toFixed(0)} W`
}

export function fmtWattHours(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—"
  if (value >= 1000) return `${(value / 1000).toFixed(1)} kWh`
  return `${value.toFixed(0)} Wh`
}

export function fmtClock(t: number, rangeSeconds: number): string {
  const date = new Date(t)
  if (rangeSeconds > 12 * 3600) {
    return date.toLocaleTimeString("en-GB", {
      hour: "2-digit",
      minute: "2-digit",
    })
  }
  return date.toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
  })
}

export function fmtClockFull(t: number): string {
  return new Date(t).toLocaleTimeString("en-GB", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  })
}

/** Short display name for an upstream/engine endpoint URL. */
export function endpointLabel(endpoint: string, index: number): string {
  const letter = String.fromCharCode(65 + index)
  try {
    const url = new URL(endpoint)
    // Loopback/IP upstreams differ by port; named services differ by host.
    if (/^[\d.[]/.test(url.hostname) || url.hostname === "localhost") {
      return `${letter} · :${url.port || "80"}`
    }
    const host = url.hostname.split(".")[0]
    return `${letter} · ${host.length > 14 ? host.slice(-14) : host}`
  } catch {
    return `engine ${letter}`
  }
}
