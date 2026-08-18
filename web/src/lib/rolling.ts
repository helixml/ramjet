type Timed = { t: number } & Record<string, number | null>

/**
 * Longest interval a single sample is allowed to stand for, as a share of the
 * averaging window. Without it, the one sample either side of a five-minute
 * data gap would carry the whole window.
 */
const MAX_GAP_SHARE = 0.25

export interface RollingSpec {
  /** Percentage (or any ratio) series to average. */
  key: string
  /**
   * Series whose magnitude weights each sample — for a hit rate, the token
   * rate the ratio was measured over. A busy second then counts for more than
   * a quiet one, which is the difference between "what share of prompt tokens
   * hit cache" and "the average of some per-sample percentages".
   */
  weightKey?: string
  /** Length of the trailing window each output point averages over. */
  windowMs: number
  /** Key the averaged value is written to on the returned rows. */
  outKey: string
}

/**
 * Adds a trailing, traffic- and time-weighted rolling average of `key`.
 *
 * Instantaneous ratios sampled over a few seconds of bursty traffic are not
 * readable as a chart: a single request that missed reads 0%, the next one
 * reads 100%, and an idle interval has no ratio at all, so the series arrives
 * as isolated points that a filled area draws as vertical hairlines. Averaging
 * over a window recovers the quantity people actually mean by "hit rate", and
 * it spans the idle gaps between bursts rather than breaking the line at each.
 *
 * Samples are weighted by their own duration as well as by `weightKey`, so a
 * series that mixes cadences — the 1 Hz live tail spliced onto 5 s or condensed
 * polled history — does not over-count its dense end.
 *
 * Rows must be ordered oldest first. Absent samples are skipped, never read as
 * zero; a window holding none of them yields `null`, because an interval with
 * no traffic is not a cold cache.
 */
export function rollingAverage(rows: Timed[], spec: RollingSpec): Timed[] {
  const { key, weightKey, windowMs, outKey } = spec
  const count = rows.length
  if (count === 0) return []
  const values: Array<number | null> = new Array(count)
  const spans = new Float64Array(count)
  const weights = new Float64Array(count)
  const maxGap = Math.max(windowMs * MAX_GAP_SHARE, 1)
  for (let index = 0; index < count; index += 1) {
    const row = rows[index]
    const value = row[key]
    values[index] = typeof value === "number" && Number.isFinite(value) ? value : null
    const gap =
      index > 0
        ? rows[index].t - rows[index - 1].t
        : count > 1
          ? rows[1].t - rows[0].t
          : windowMs
    const span = Math.min(Math.max(gap, 1), maxGap)
    spans[index] = span
    const weight = weightKey == null ? null : row[weightKey]
    weights[index] =
      typeof weight === "number" && Number.isFinite(weight) && weight > 0 ? span * weight : 0
  }

  const out: Timed[] = new Array(count)
  let start = 0
  let weighted = 0
  let weight = 0
  let spanned = 0
  let span = 0
  for (let index = 0; index < count; index += 1) {
    const value = values[index]
    if (value != null) {
      weighted += weights[index] * value
      weight += weights[index]
      spanned += spans[index] * value
      span += spans[index]
    }
    const from = rows[index].t - windowMs
    while (start < index && rows[start].t < from) {
      const dropped = values[start]
      if (dropped != null) {
        weighted -= weights[start] * dropped
        weight -= weights[start]
        spanned -= spans[start] * dropped
        span -= spans[start]
      }
      start += 1
    }
    // The weighted mean is the real answer; the unweighted one covers a window
    // whose traffic series is absent or all zero, where the ratios are still
    // the only thing known about it.
    const average = weight > 1e-9 ? weighted / weight : span > 1e-9 ? spanned / span : null
    out[index] = { ...rows[index], [outKey]: average }
  }
  return out
}

/** Compact label for an averaging window, e.g. `45s`, `3m`, `1h`. */
export function windowLabel(windowMs: number): string {
  const seconds = Math.round(windowMs / 1000)
  if (seconds < 90) return `${seconds}s`
  const minutes = Math.round(seconds / 60)
  if (minutes < 90) return `${minutes}m`
  return `${Math.round(minutes / 60)}h`
}
