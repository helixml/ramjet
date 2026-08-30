type Timed = { t: number } & Record<string, number | null>

export interface BandKeys {
  lowKey: string
  highKey: string
}

/**
 * Collapses `rows` to at most `buckets` even time slices, keeping in each the
 * single row where the plotted series peak.
 *
 * Serving rates are bursty: at 1 Hz a fleet doing 100k tok/s in one second
 * reads 0 in the next, and a window holding more samples than the card has
 * pixels draws every one of them into the same column. The line still shows
 * the spike, but the crosshair snaps to whichever sample is nearest the
 * cursor, so pointing at a 100k spike reports 0 far more often than not —
 * correct about a sample nobody asked about, and useless as a readout.
 *
 * Keeping one real row per column makes the chart and its tooltip agree by
 * construction: the value under the cursor is the value drawn there. Peaks
 * survive rather than being averaged away, and because a whole row is chosen
 * rather than a per-series maximum, the tooltip shows measurements that were
 * taken at the same instant instead of a composite that never occurred.
 *
 * Idle stays idle: a column whose rows are all zero picks a zero, and one
 * with no samples at all contributes no point. Rows must be ordered oldest
 * first. `band` low/high are reduced to the column's own envelope, which is
 * what an envelope already means.
 */
export function peakBuckets(
  rows: Timed[],
  keys: string[],
  buckets: number,
  band?: BandKeys,
): Timed[] {
  if (rows.length <= buckets || buckets < 1 || keys.length === 0) return rows
  const first = rows[0].t
  const span = rows[rows.length - 1].t - first
  if (!(span > 0)) return rows
  const width = span / buckets

  const out: Timed[] = []
  let index = 0
  while (index < rows.length) {
    const bucket = Math.min(buckets - 1, Math.floor((rows[index].t - first) / width))
    let best = index
    let bestScore: number | null = null
    let low: number | null = null
    let high: number | null = null
    while (
      index < rows.length &&
      Math.min(buckets - 1, Math.floor((rows[index].t - first) / width)) === bucket
    ) {
      const row = rows[index]
      let score: number | null = null
      for (const key of keys) {
        const value = row[key]
        if (typeof value === "number" && Number.isFinite(value)) {
          score = (score ?? 0) + Math.abs(value)
        }
      }
      // A row carrying any measurement beats one carrying none, so a column
      // is only empty when every sample in it was.
      if (score != null && (bestScore == null || score > bestScore)) {
        bestScore = score
        best = index
      }
      if (band) {
        const rowLow = row[band.lowKey]
        const rowHigh = row[band.highKey]
        if (typeof rowLow === "number" && Number.isFinite(rowLow)) {
          low = low == null ? rowLow : Math.min(low, rowLow)
        }
        if (typeof rowHigh === "number" && Number.isFinite(rowHigh)) {
          high = high == null ? rowHigh : Math.max(high, rowHigh)
        }
      }
      index += 1
    }
    out.push(
      band ? { ...rows[best], [band.lowKey]: low, [band.highKey]: high } : rows[best],
    )
  }
  return out
}
