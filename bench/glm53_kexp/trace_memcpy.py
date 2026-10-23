#!/usr/bin/env python3
"""Memcpy/memset activity in a trace, and how much of it overlaps kernels matching a regex."""
import collections, gzip, json, re, sys
tr = json.load(gzip.open(sys.argv[1], "rt"))
pat = re.compile(sys.argv[2], re.I)
ev = [e for e in tr["traceEvents"] if e.get("ph") == "X"]
mem = [e for e in ev if e.get("cat") in ("gpu_memcpy", "gpu_memset")]
tgt = [e for e in ev if e.get("cat") == "kernel" and pat.search(e["name"])]
by = collections.defaultdict(lambda: [0, 0.0, 0])
for m in mem:
    a = m.get("args", {})
    key = (m["name"], a.get("stream"))
    by[key][0] += 1; by[key][1] += m["dur"]; by[key][2] += a.get("bytes", 0) or 0
print("memcpy/memset by (name, stream):")
for (n, s), (c, d, b) in sorted(by.items(), key=lambda x: -x[1][1])[:10]:
    print(f"  n={c:6d} {d/1000:8.1f} ms {b/1e6:9.1f} MB stream={s} {n}")
ov = 0.0; ovb = collections.Counter()
for t in tgt:
    t0, t1 = t["ts"], t["ts"] + t["dur"]
    for m in mem:
        o = min(t1, m["ts"] + m["dur"]) - max(t0, m["ts"])
        if o > 0:
            ov += o; ovb[m["name"]] += o
tot = sum(t["dur"] for t in tgt)
print(f"target {tot/1000:.1f} ms; overlapped by memcpy {ov/1000:.1f} ms ({ov/max(tot,1):.0%})", dict((k, round(v/1000,1)) for k, v in ovb.most_common(4)))
