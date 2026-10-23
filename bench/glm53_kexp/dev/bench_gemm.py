"""Small-M block-FP8 GEMM shootout at GLM-5.3-Flash TP2 per-rank shapes.

Every candidate is timed inside a CUDA graph (as the engine runs decode), so
launch overhead is excluded and only device time is compared.
"""
import argparse
import sys

import torch

sys.path.insert(0, "/opt/sglang-source/python")

SHAPES = {  # name: (N, K, calls per decode step)
    "kda_qkv": (12288, 4096, 34),
    "kda_o": (4096, 4096, 34),
    "mla_qkv_a": (2048, 4096, 11),
    "mla_q_b": (8192, 1536, 11),
    "mla_o": (4096, 8192, 11),
    "shexp_gate_up": (2048, 4096, 42),
    "shexp_down": (4096, 1024, 42),
    "lm_head": (77440, 4096, 1),
}


def make(N, K, dev):
    w = (torch.randn(N, K, device=dev) * 0.5).to(torch.float8_e4m3fn)
    s = torch.rand((N + 127) // 128, K // 128, device=dev) * 0.02 + 0.001
    return w, s


def ref(x, w, s):
    N, K = w.shape
    sf = s.repeat_interleave(128, 0)[:N].repeat_interleave(128, 1)[:, :K]
    return (x.float() @ (w.float() * sf).T)


def graph_time(fn, iters=20, reps=5):
    s = torch.cuda.Stream()
    s.wait_stream(torch.cuda.current_stream())
    with torch.cuda.stream(s):
        for _ in range(3):
            fn()
    torch.cuda.current_stream().wait_stream(s)
    g = torch.cuda.CUDAGraph()
    with torch.cuda.graph(g):
        for _ in range(iters):
            fn()
    g.replay()
    torch.cuda.synchronize()
    best = 1e9
    for _ in range(reps):
        a = torch.cuda.Event(enable_timing=True)
        b = torch.cuda.Event(enable_timing=True)
        a.record()
        g.replay()
        b.record()
        torch.cuda.synchronize()
        best = min(best, a.elapsed_time(b) / iters)
    return best * 1000  # us


def candidates(N, K, dev, which, tl_cfg):
    from sglang.srt.layers.quantization import fp8_utils

    out = {}
    if "cutlass" in which:
        out["cutlass"] = lambda x, w, s: fp8_utils.cutlass_w8a8_block_fp8_linear_with_fallback(x, w, [128, 128], s)
    if "triton" in which:
        out["triton"] = lambda x, w, s: fp8_utils.triton_w8a8_block_fp8_linear(x, w, [128, 128], s)
    if "mod" in which:
        import w8a16_tilelang as mod

        out["module"] = lambda x, w, s: mod.w8a16_block_fp8_linear(x, w, s)
    if "tl" in which:
        import w8a16_tl

        op = w8a16_tl.W8A16(N, K, dev, **tl_cfg)
        out["tilelang"] = lambda x, w, s: op(x, w, s)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shapes", default=",".join(SHAPES))
    ap.add_argument("--ms", default="1,6,12,24")
    ap.add_argument("--which", default="cutlass,triton,tl")
    ap.add_argument("--bn", type=int, default=64)
    ap.add_argument("--split", type=int, default=0, help="0 = auto")
    ap.add_argument("--stages", type=int, default=3)
    ap.add_argument("--threads", type=int, default=128)
    ap.add_argument("--bm", type=int, default=16)
    ap.add_argument("--rs", action="store_true")
    ap.add_argument("--v2", action="store_true")
    ap.add_argument("--bk", type=int, default=128)
    ap.add_argument("--waves", type=float, default=2)
    a = ap.parse_args()
    dev = "cuda:0"
    torch.cuda.set_device(0)
    sms = torch.cuda.get_device_properties(0).multi_processor_count
    for name in a.shapes.split(","):
        N, K, per_step = SHAPES[name]
        w, s = make(N, K, dev)
        split = a.split
        if split == 0:
            tiles = N // a.bn
            kb = K // a.bk
            split = 1
            while tiles * split < a.waves * sms and kb % (split * 2) == 0 and split < 16:
                split *= 2
        cfg = dict(BM=a.bm, BN=a.bn, BK=a.bk, split=split, stages=a.stages, threads=a.threads, rs=a.rs, v2=a.v2)
        cands = candidates(N, K, dev, a.which, cfg)
        # Engine weights are cold (the whole model streams between uses), so
        # rotate through enough copies to defeat the 128MB L2.
        ncopy = max(1, -(-384 * 2**20 // (N * K)))
        ws = [make(N, K, dev) for _ in range(ncopy)]
        w, s = ws[0]
        roof_us = N * K / 1.6e12 * 1e6
        for m in [int(v) for v in a.ms.split(",")]:
            x = torch.randn(m, K, device=dev, dtype=torch.bfloat16)
            r = ref(x, w, s)
            row = [f"{name:14s} N={N:6d} K={K:5d} M={m:3d} roof@1.6TB/s={roof_us:7.1f}us"]
            for cname, fn in cands.items():
                try:
                    y = fn(x, w, s)
                    err = ((y.float() - r).abs().max() / r.abs().max()).item()
                    it = [0]

                    def step():
                        ww, ss = ws[it[0] % ncopy]
                        it[0] += 1
                        fn(x, ww, ss)

                    t = graph_time(step, iters=max(20, ncopy))
                    row.append(f"{cname}={t:7.1f}us(err {err:.1e})")
                except Exception as e:  # noqa: BLE001
                    row.append(f"{cname}=FAIL({type(e).__name__}: {str(e)[:120]})")
            row.append(f"tl_cfg=bn{a.bn}/split{split}")
            print("  ".join(row), flush=True)


if __name__ == "__main__":
    main()
