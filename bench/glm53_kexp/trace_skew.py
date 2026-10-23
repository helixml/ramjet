#!/usr/bin/env python3
"""Pair the i-th all-reduce kernel across two rank traces: start skew, end skew, and
what the late rank was doing just before (gap vs kernels)."""
import gzip, json, re, statistics, sys

def load(p):
    ev = json.load(gzip.open(p, "rt"))["traceEvents"]
    k = sorted([e for e in ev if e.get("ph") == "X" and e.get("cat") == "kernel"], key=lambda e: e["ts"])
    return k

pat = re.compile(sys.argv[3] if len(sys.argv) > 3 else "AllReduce|remote_push", re.I)
k0, k1 = load(sys.argv[1]), load(sys.argv[2])
a0 = [e for e in k0 if pat.search(e["name"])]
a1 = [e for e in k1 if pat.search(e["name"])]
n = min(len(a0), len(a1))
st = [a1[i]["ts"] - a0[i]["ts"] for i in range(n)]
en = [(a1[i]["ts"] + a1[i]["dur"]) - (a0[i]["ts"] + a0[i]["dur"]) for i in range(n)]
d0 = [a0[i]["dur"] for i in range(n)]
d1 = [a1[i]["dur"] for i in range(n)]
print(f"pairs={n}  TP0 dur p50={statistics.median(d0):.0f}us  TP1 dur p50={statistics.median(d1):.0f}us")
print(f"start skew TP1-TP0 p10/p50/p90 = {sorted(st)[n//10]:.0f}/{statistics.median(st):.0f}/{sorted(st)[9*n//10]:.0f} us")
print(f"end   skew TP1-TP0 p10/p50/p90 = {sorted(en)[n//10]:.0f}/{statistics.median(en):.0f}/{sorted(en)[9*n//10]:.0f} us")
# lower bound on the transfer itself: time from the later rank's start to the common end
xfer = [min(a0[i]["ts"] + a0[i]["dur"], a1[i]["ts"] + a1[i]["dur"]) - max(a0[i]["ts"], a1[i]["ts"]) for i in range(n)]
print(f"time after BOTH ranks entered (transfer) p50 = {statistics.median(xfer):.0f} us")
# idle gaps on each rank in the window
def gaps(k):
    g = 0.0; end = k[0]["ts"] + k[0]["dur"]
    for e in k[1:]:
        if e["ts"] > end: g += e["ts"] - end
        end = max(end, e["ts"] + e["dur"])
    return g / 1000
print(f"GPU idle gaps: TP0 {gaps(k0):.1f} ms, TP1 {gaps(k1):.1f} ms")
