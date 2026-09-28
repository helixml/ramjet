# GLM-5.3-Flash snapshot ROI

2026-09-26. Would GLM conversation state that survives outside GPU memory for
hours or days avoid enough re-prefill to justify a CPU/NVMe KDA snapshot tier?

**Finding: A — little benefit.** Measured on 13.6 days of production traffic
(30,557 GLM requests, 3.44B prompt tokens), the engines already served 98.58%
of prompt tokens from cache. Infinite retention on either replica would have
avoided at most another 0.44% (15.3M tokens, 1.1M/day, 380 GPU-seconds/day,
0.11% of the four GLM GPUs). The benefit plateaus within 30 minutes, not hours:
75% of it needed under 5 minutes of retention and 94.5% under 30. Since the
2026-09-25 per-path cap plus HiCache rollout, the recoverable share fell to
0.025% on the serving replica and 0.125% on either replica. No NVMe snapshot
tier is warranted. Re-run the analysis once a week or more of post-rollout
traffic has accumulated (below); the decision would change only if that data
contradicts the curve shape shown here.

## Data and method

Source: node06's route-journal archive (`/var/lib/ramjet-journal`), LB
containers whose Compose file contains `glm53`, served upstreams 1 and 2
(`glm53sm120-b`, `glm53sm120-c`), 2026-09-12 19:22 to 2026-09-26 10:05 UTC.
Excluded: the 87 requests of the 2026-09-26 07:00-07:13 synthetic long-prompt
lane replay, which ran through the live LB and would otherwise supply 80% of the
post-rollout "misses". Also excluded: 1,910 requests without response usage,
555 HTTP 400s, and 104 client disconnects.

```bash
python3 bench/snapshot_roi.py journal.sqlite3 --upstreams 1,2 \
  --compose-match glm53 \
  --exclude 2026-09-26T07:00:00..2026-09-26T07:13:00 \
  [--until|--since 2026-09-25T15:12:00]
```

The journal has no prompts, fingerprints, or session identity, so the tool
infers the counterfactual from two recorded facts.

- **Block ages.** Each start record carries, per candidate replica, the age of
  every leading 2 KiB block that the LB has seen served (`overlap_ages_ms`,
  journal v11+). The age is measured from that replica's last completed
  request containing the block.
- **Engine-reported usage.** Each finish record carries the engine's
  `prompt_tokens`, `cached_tokens`, and completion instant.

A follow-up turn's deepest block was last served by the finish at
`start - age`. 97% of requests match such a finish within ±1ms. When the match
covers the predecessor's whole prompt, the request is a continuation. The
predecessor's engine-reported prompt then becomes the prefix a snapshot could
have supplied: warm continuations cache within -0.65%/+2.2% (5th/95th
percentile) of exactly that.

- **Retention horizon H.** A continuation is credited only if its idle gap is
  at most H. An unlinked request is credited the blocks younger than H,
  converted at its own bytes/token ratio (median 4.2 bytes/token).
- **Noise floor.** A shortfall counts as avoidable only above
  max(2,048 tokens, 2% of the prompt).
- **Same vs any replica.** `same_replica` credits only the replica that
  served the request. `any_replica` also credits the peer, which models a
  tier shared by both engines on the host.

The LB's prefix index is an LRU of 100,000 blocks per upstream. GLM never
exceeded 35,507 new blocks on one upstream in one LB lifetime, so the index
did not truncate any history. The binding limit is the LB container's
lifetime (longest 90.5h): a recreate empties the index. Gaps longer than that
are censored rather than absent, and the `censored` column counts the
requests whose LB was younger than each horizon.

## 1. Existing observability (audit)

| needed | recorded? | where |
|---|---|---|
| request identity | `seq`, process-local; `(container, seq)` in the archive | `src/journal.rs` |
| timestamp | `unix_ms` on start and finish | journal |
| model | no; derive from the upstream ordinal and `RJ_UPSTREAM_MODELS` | compose |
| selected engine | `chosen`, `served_chosen`, finish `upstream` | journal |
| prompt / cached tokens | finish `prompt_tokens`, `cached_tokens` (engine usage) | `src/usage.rs` |
| candidates + overlap per candidate | `candidates[].overlap_blocks`, `stale_blocks` | journal |
| block age / residency estimate | `candidates[].overlap_ages_ms` (≤64 runs) | `src/affinity_horizon.rs` |
| routing reason | `outcome`, session/horizon/lane objects | journal |
| TTFT | finish `ttft_ms` (streaming, from arrival) | journal |
| prefill duration | no; TTFT is the proxy | — |
| conversation identity | no (by design); inferred from block ages above | — |

Prometheus already exports the aggregates: `ramjet_model_prompt_tokens_total`,
`ramjet_model_cached_prompt_tokens_total`, `ramjet_cache_requests_total{outcome}`,
`ramjet_cache_ttft_seconds`, `ramjet_route_overlap_blocks`, and
`ramjet_route_affinity_horizon_seconds`. Ramjet does not scrape SGLang's
state-pool or HiCache counters.

## 2. Added or fixed observability

No new Rust telemetry was needed: the v11/v12 journal already answers every
question above. Three things were broken or missing around it.

- **Collection had stopped.** The collector installed on node06 accepted only
  journal v1-11. From the v0.6.2 LB rollout (2026-09-25 15:12) every
  five-minute run failed with `route-journal version is unsupported`. The
  current `main` copy was installed on 2026-09-26 at 10:04 UTC, with the old
  file kept as `.v11-backup-20260926`. The 19 hours from the stopped container
  `bb2d425b7967` were recovered by ID, 2,843 records.
- **The daily cost audit skipped current records.** `serving_cost_audit.py`
  marked every v11/v12 output-limit object `invalid`, although the schema is
  unchanged since v7.
- **No guard against version drift.** `bench/test_snapshot_roi.py` now checks
  that the archive, `route_replay.py`, and the cost audit all accept the
  `VERSION` in `src/journal.rs`. A bump now fails CI until every consumer is
  updated. Re-installing the host collector remains an operational step,
  documented in `deploy/qwen38_flash_next/README.md`.

`bench/snapshot_roi.py` produces the horizon replay, miss analysis, and tier
sizing below.

## 3. Retention-horizon replay

Before the rollout: 2026-09-12 to 2026-09-25 15:12, 12.83 days, 29,152
requests. Prompt 3.21B tokens, cached 98.57%, uncached 45.9M (3.57M/day).

| retention | helped, same | avoidable, same | % prompt | helped, any | avoidable, any | % prompt | capture of ∞ (any) |
|---|---|---|---|---|---|---|---|
| current | — | 0 | 0 | — | 0 | 0 | — |
| 5m | 445 | 8.39M | 0.261 | 518 | 11.18M | 0.348 | 74.7% |
| 15m | 473 | 10.45M | 0.326 | 540 | 12.84M | 0.400 | 85.8% |
| 30m | 497 | 11.33M | 0.353 | 564 | 14.13M | 0.440 | 94.4% |
| 1h | 512 | 12.19M | 0.380 | 576 | 14.35M | 0.447 | 95.8% |
| 3h | 518 | 12.66M | 0.394 | 582 | 14.45M | 0.450 | 96.5% |
| 6h | 524 | 12.73M | 0.396 | 587 | 14.51M | 0.452 | 96.9% |
| 12h / 24h / 3d / 7d / ∞ | 529 | 13.19M | 0.411 | 592 | 14.97M | 0.466 | 100% |

After the rollout: 2026-09-25 15:12 to 2026-09-26 10:05, 0.79 days, 1,405
requests. Prompt 229M tokens, cached 98.71%, uncached 2.95M.

| retention | helped, same | avoidable, same | % prompt | helped, any | avoidable, any | % prompt |
|---|---|---|---|---|---|---|
| 5m | 10 | 40.9k | 0.018 | 15 | 270.7k | 0.118 |
| 15m … ∞ | 11 | 57.1k | 0.025 | 16 | 287.0k | 0.125 |

Of the 45.9M tokens the engines re-prefilled before the rollout:

| share | tokens | what it is |
|---|---|---|
| recoverable by any retention | 15.0M | the rows above |
| new continuation tokens | 19.8M | the turn's growth beyond its predecessor; never seen before |
| new conversations, forks, returns | 9.0M | first turns and branches |
| unlinked within an hour of an LB start | 0.45M | possibly lost returns |
| below the noise floor | 1.6M | |

After the rollout the recoverable share is 0.29M of 2.95M, most of it
cross-replica.

## 4. Coding / agent conversations

Continuation linkage infers 1,709 conversations. The median has 7 turns and
the 90th percentile 36.

Idle gap between linked turns, all 28,848 continuations:

| <5m | 5-30m | 30m-1h | 1-6h | 6-24h | 1-3d | 3-7d | >7d |
|---|---|---|---|---|---|---|---|
| 28,400 | 281 | 36 | 105 | 25 | 1 | 0 | 0 |

The "idle for four hours, then turn 5" pattern is real but rare: 131 turns
came back after more than an hour, and 115 of them still hit in GPU cache.
At this traffic level the 500k-token pool keeps idle state for hours.

Misses, by the shortest retention that would have recovered them (all
windows, either replica, infinite horizon):

| gap | requests | tokens | cross-replica |
|---|---|---|---|
| <5m | 533 | 11.55M | 97 |
| 5-30m | 47 | 2.97M | 1 |
| 30m-1h | 12 | 0.18M | 0 |
| 1-6h | 11 | 0.10M | 2 |
| 6-24h | 5 | 0.46M | 0 |
| ≥1d | 0 | 0 | 0 |

Misses by recoverable prefix length:

| <8k | 8-32k | 32-64k | 64-128k | 128-256k | ≥256k |
|---|---|---|---|---|---|
| 5 req / 0.02M | 335 / 2.07M | 184 / 1.94M | 43 / 1.96M | 21 / 3.73M | 20 / 5.53M |

Conversation age at each turn: 15,639 turns under 5m, 7,102 at 5-30m, 2,405
at 30m-1h, 3,260 at 1-6h, 424 at 6-24h, and 18 at 1-3d.

The pre-rollout misses were device-capacity misses, not retention misses. The
lost prefix was typically seconds to minutes old, the textbook sign of the
28-slot state pool thrashing that the 2026-09-25 entry in `EXPERIMENTS.md`
diagnosed. A ≥100k-token cold prefill between the two turns on the same
replica raised the miss rate from 0.7% to 23% (7 of 31). The per-path cap and
HiCache removed that class: after the rollout only 6 same-replica
continuations missed, 21.5k tokens in total.

The tail is still where it hurts. Twenty 256k+-token turns carry 36% of the
recoverable tokens. The miss TTFT median was 3.1s (p90 10.3s) against a 0.9s
warm median. A tier that helps those turns must be large enough to hold them,
which item 6 below prices.

## 5. Snapshot size (verified on the running build)

From `glm53sm120-b`'s allocation log (image `sha256:899fe8eb…`, TP2; SGLang
reports GiB):

| component | per TP rank | per replica (2 ranks) |
|---|---|---|
| KDA/Mamba state per slot: conv 0.07 + ssm 0.99 GiB / 28 slots | 40.7 MB | 81.3 MB |
| MLA KV incl. DSA indexer: 3.38 GiB / 499,968 tokens | 7,259 B/token | 14.5 KB/token |
| EAGLE draft KV: 0.31 GiB / 499,968 | 666 B/token | 1.3 KB/token |
| speculative intermediate SSM/conv buffers, 1.10 GiB | not snapshot state | — |

The intermediate buffers explain the earlier "~77MB per slot" figure: the
snapshot state itself is 40.7 MB per rank. MLA's latent KV is replicated
across TP ranks, so a deduplicating store needs about half the KV bytes shown.
HiCache stores it per rank.

A conversation snapshot at L tokens costs 81.3 MB plus L × 15.85 KB. KV
therefore dominates beyond about 5k tokens: a 300k-token conversation is
4.8 GB, of which KDA state is 1.7%. A "KDA snapshot" tier is really a
KV-plus-state tier.

## 6. Snapshots needed

The tool sizes a TTL tier that holds one tail per conversation (prompt plus
completion, state plus KV). A tail leaves the tier when a continuation
consumes it or when it outlives the horizon. The whole 13.6-day window is
used, so tiers longer than the LB lifetime are upper bounds.

| retention | every tail: peak | every tail: mean | tails later reused: peak | tails later reused: mean |
|---|---|---|---|---|
| 5m | 60 / 25 GB | 0.9 GB | 12 / 11.7 GB | 0.2 GB |
| 30m | 166 / 69 GB | 3.5 GB | 14 / 16.1 GB | 0.5 GB |
| 1h | 243 / 108 GB | 6.5 GB | 14 / 16.1 GB | 0.6 GB |
| 6h | 668 / 337 GB | 34 GB | 15 / 16.1 GB | 1.5 GB |
| 24h | 1,006 / 543 GB | 129 GB | 16 / 16.1 GB | 1.9 GB |
| 3d | 1,396 / 897 GB | 319 GB | 16 / 16.1 GB | 1.9 GB |
| 7d | 1,652 / 1.29 TB | 512 GB | 16 / 16.1 GB | 1.9 GB |

The snapshots that would ever be read peak at 16 GB, which fits in RAM. A
blind 24h-7d TTL tier would need 0.5-1.3 TB of NVMe (node06 has 1.17 TB free
on `/prod`). More than 97% of that would never be read again. At 4.8 GB per
long snapshot, 1 TB holds about 200 300k-token conversations or 2,500
20k-token ones.

## 7. Restore economics

Cold prefill: the production median is 5,420 tokens/s on the ≥16k-token misses
(TTFT-based, n=446). That matches the 2026-09-12 bench figure of 5.2-6.0k.

- **Host → GPU.** HiCache measured 0.43s for a 20k-token session (0.40 GB),
  0.93 GB/s effective.
- **NVMe → host.** Modelled at 5 GB/s: two Micron 7450 3.2TB drives, rated
  6.8 GB/s sequential, less ZFS overhead.

| context | cold prefill | snapshot bytes | host→GPU | + NVMe read | speed-up |
|---|---|---|---|---|---|
| 8k | 1.51s | 0.21 GB | 0.23s | 0.27s | 5.6× |
| 20k | 3.69s | 0.40 GB | 0.43s | 0.51s | 7.2× |
| 64k | 12.1s | 1.12 GB | 1.21s | 1.43s | 8.4× |
| 128k | 24.2s | 2.16 GB | 2.33s | 2.76s | 8.8× |
| 300k | 55.4s | 4.84 GB | 5.22s | 6.19s | 8.9× |

Restore costs about 20 µs per token against 184 µs per token for prefill.
Break-even is L = a × 6,090 tokens/s, where a is the fixed per-restore
overhead: 600 tokens at a = 0.1s, 3,000 at 0.5s. Economics per restore are
not the constraint. The volume of restorable misses is.

## 8. Answers

1. **Are we cache constrained?** No. 1.42% of GLM prompt tokens were
   re-prefilled, and about two thirds of those (31.0M tokens) were never-seen
   content. The part a retention tier could have saved was 0.47% before the
   rollout and is 0.13% since.
2. **What retention period matters?** Before the rollout: 5m captured 75%,
   30m 94.4%, 1h 95.8%, 6h 96.9%, 12h 100%. After it: 5m 94%, 15m 100%.
   Nothing past 24h was observable within the LB lifetime, and only one
   continuation had a 1-3 day gap.
3. **How much GPU work?** At most 1.17M tokens/day, 396 GPU-seconds/day
   (0.11% of four GPUs) and 173s/day of summed TTFT over about 46
   requests/day, before the rollout. Since the rollout: 365k tokens/day, 124
   GPU-s/day, and 54s of TTFT per day across about 20 requests. The median
   helped request saves 0.7s; the 90th percentile saves 5-7s.
4. **How many snapshots?** Snapshots actually reused peak at 16 (16 GB),
   against 0.5-1.3 TB for a blind 24h-7d TTL tier.
5. **Where does it plateau?** Before 1 hour. The curve does not rise after
   GPU-resident state would normally disappear. It rises only during the
   seconds-to-minutes window where the device pool thrashed, which the
   rollout addressed.

## Decision

**A. Little benefit.** The GPU pool plus the 2026-09-25 host tier already
capture effectively every reusable prefix. The only visible pattern is the
long-conversation tail, and it is a capacity problem measured in minutes
rather than hours. Two levers fit it better than persistence:

- **Replica placement.** The long-prompt lane showed that two ~290k-token
  conversations on one replica overflow its 500k-token pool.
- **A larger `--hicache-size`,** if cross-replica or post-restart misses grow.

A shared host tier would capture the cross-replica 0.1%.

Revisit only if any of these changes:

- the weekly re-run shows ≥1% of prompt tokens recoverable at horizons ≥1h;
- the traffic mix gains days-idle resumable sessions, e.g. user-facing
  coding chats rather than bot agents;
- engine restarts become frequent enough that restart-surviving state
  matters.

The re-run command above works on the live archive, which now collects again.
For a longer window, run it over the daily `segments/*/*.jsonl.gz`.

## If it ever looks promising (not now)

The next experiment would extend what exists rather than start a new
serializer. SGLang's HiCache already writes KV and Mamba/KDA state
host-through (`hicache_backup_tokens_total`), and `--hicache-storage-backend`
adds an L3 tier beneath it. The steps would be:

1. On one isolated replica, enable a file/NVMe storage backend with the
   unified (KV + Mamba) cache.
2. Verify that state pages are backed up, not just KV.
3. Replay the long-conversation tail with `bench/prefix_recall_probe.py`,
   adding an idle interval longer than the host tier's residency.
4. Measure restore latency against the table in section 7.

Size capacity from the reused-tail column, not the blind TTL column.

## Caveats

- **Inference, not a join.** Linking relies on block ages. 5.6% of requests
  have no link (new conversations, forks, returns after an LB recreate).
  Crediting all 37 unlinked requests with ≥32k uncached tokens as lost returns
  would add at most 6.1M tokens (0.18%).
- **Conservative credit.** The predecessor's prompt ignores the engine's reuse
  of prior output tokens. Unlinked credit converts bytes to tokens at the
  request's own ratio, which underestimates the engine by about 1% at the
  median.
- **Short post-rollout sample.** 0.79 days and 1,405 requests, with one
  synthetic window removed. Pre-rollout traffic is the stronger, and
  pessimistic, evidence.
- **Engine restarts** empty the device cache but not the LB index. Their
  misses are included and appear as short-gap misses.
- **Coverage.** Traffic that bypasses the LB is invisible, as is 6% of GLM
  requests that returned no usage.
