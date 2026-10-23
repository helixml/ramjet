#!/usr/bin/env python3
"""For kernels matching a regex: stream, launch shape, and how much other GPU work overlaps them."""
import collections, gzip, json, re, sys
tr = json.load(gzip.open(sys.argv[1], "rt"))
pat = re.compile(sys.argv[2], re.I)
k = [e for e in tr["traceEvents"] if e.get("ph") == "X" and e.get("cat") == "kernel"]
k.sort(key=lambda e: e["ts"])
target = [e for e in k if pat.search(e["name"])]
others = [e for e in k if not pat.search(e["name"])]
streams = collections.Counter((e.get("args", {}).get("stream"), e["tid"]) for e in target)
print("target kernels:", len(target), "streams(stream,tid):", dict(streams))
print("other-kernel streams:", dict(collections.Counter(e.get("args", {}).get("stream") for e in others).most_common(6)))
a = target[0].get("args", {})
print("shape:", {x: a.get(x) for x in ("grid", "block", "shared memory", "registers per thread")})
# overlap: fraction of each target kernel's duration covered by other kernels
import bisect
starts = [e["ts"] for e in others]
tot = cov = 0.0
names = collections.Counter()
for t in target:
    t0, t1 = t["ts"], t["ts"] + t["dur"]
    tot += t["dur"]
    i = bisect.bisect_left(starts, t0 - 20000)
    for o in others[i:]:
        if o["ts"] > t1:
            break
        ov = min(t1, o["ts"] + o["dur"]) - max(t0, o["ts"])
        if ov > 0:
            cov += ov
            names[o["name"][:70]] += ov
print(f"target time {tot/1000:.1f} ms, overlapped by other kernels {cov/1000:.1f} ms ({cov/tot:.0%})")
for n, v in names.most_common(6):
    print(f"  {v/1000:8.1f} ms  {n}")
# what runs right before each target on its stream / the gap before it
durs = sorted(t["dur"] for t in target)
print("dur p10/p50/p90 us:", durs[len(durs)//10], durs[len(durs)//2], durs[9*len(durs)//10])
