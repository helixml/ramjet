#!/usr/bin/env python3
"""Summarize GPU kernel time from a torch profiler chrome trace (json or json.gz).

Prints total GPU busy time, top kernels by summed duration, and a coarse
category breakdown so a GLM-5.3 decode/prefill step can be attributed to
MoE / FP8 GEMM / KDA / DSA / mHC / comms / other.
"""
import collections
import gzip
import json
import re
import sys

CATS = [
    ("comm", r"nccl|allreduce|all_reduce|pcie_ipc|cross_device|reduce_scatter|allgather"),
    ("moe", r"b12x|moe|w4a16|expert|topk_softmax|grouped_topk|routing|fused_topk|biased_grouped"),
    ("dsa_attn", r"sparse_mla|flashmla|mla|indexer|mqa_logits|kpool|fp8_paged|fast_topk|topk_transform|dsa"),
    ("kda", r"kda|fla_|chunk_|recurrent|gated_delta|causal_conv|conv1d|fused_sigmoid_gating|l2norm|solve_tril|wy_|cumsum"),
    ("mhc", r"mhc|sinkhorn|hc_pre|hc_post|hc_split|hc_contract|big_fuse"),
    ("fp8_gemm", r"blockwise|fp8_blockwise|per_token_group_quant|sgl_per_token|fp8.*gemm|gemm.*fp8|w8a8"),
    ("gemm_bf16", r"gemm|cublas|cutlass|xmma|sm80_|sm90_|ampere|nvjet|splitk|matmul"),
    ("norm", r"rms|norm"),
    ("sampling", r"sampl|argmax|softmax|top_p|top_k|verify|tree|eagle|spec"),
]


def cat_of(name):
    low = name.lower()
    for c, pat in CATS:
        if re.search(pat, low):
            return c
    return "other"


def load(path):
    op = gzip.open if path.endswith(".gz") else open
    with op(path, "rt") as f:
        return json.load(f)


def main():
    path = sys.argv[1]
    top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
    tr = load(path)
    ev = tr["traceEvents"] if isinstance(tr, dict) else tr
    k = [e for e in ev if e.get("ph") == "X" and e.get("cat") in ("kernel", "gpu_memcpy", "gpu_memset")]
    if not k:
        print("no kernel events")
        return
    by = collections.defaultdict(lambda: [0.0, 0])
    cats = collections.defaultdict(float)
    for e in k:
        n = e["name"]
        by[n][0] += e["dur"]
        by[n][1] += 1
        cats[cat_of(n)] += e["dur"]
    total = sum(v[0] for v in by.values())
    t0 = min(e["ts"] for e in k)
    t1 = max(e["ts"] + e["dur"] for e in k)
    print(f"kernels={len(k)} busy={total/1000:.1f}ms span={(t1-t0)/1000:.1f}ms util={total/(t1-t0):.0%}")
    print("\n== categories ==")
    for c, v in sorted(cats.items(), key=lambda x: -x[1]):
        print(f"{c:10s} {v/1000:9.2f}ms {v/total:6.1%}")
    print(f"\n== top {top} kernels ==")
    for n, (d, c) in sorted(by.items(), key=lambda x: -x[1][0])[:top]:
        print(f"{d/1000:9.2f}ms {d/total:6.1%} n={c:6d} avg={d/c:8.1f}us [{cat_of(n)}] {n[:150]}")


if __name__ == "__main__":
    main()
