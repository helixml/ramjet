#!/usr/bin/env python3
"""Group kernels matching a regex by (grid, block) to recover GEMM shapes."""
import collections, gzip, json, re, sys
tr = json.load(gzip.open(sys.argv[1], "rt"))
pat = re.compile(sys.argv[2], re.I)
by = collections.defaultdict(lambda: [0.0, 0])
for e in tr["traceEvents"]:
    if e.get("ph") == "X" and e.get("cat") == "kernel" and pat.search(e["name"]):
        a = e.get("args", {})
        key = (e["name"][:60], str(a.get("grid")), str(a.get("block")), a.get("shared memory"), a.get("registers per thread"))
        by[key][0] += e["dur"]; by[key][1] += 1
for k, (d, c) in sorted(by.items(), key=lambda x: -x[1][0])[:30]:
    print(f"{d/1000:8.2f}ms n={c:5d} avg={d/c:7.1f}us grid={k[1]} block={k[2]} smem={k[3]} regs={k[4]} {k[0]}")
