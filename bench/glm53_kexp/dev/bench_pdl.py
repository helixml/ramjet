"""v2 vs v3 (PDL + L2 weight prefetch) with a real producer kernel before each GEMM."""
import sys
import torch
sys.path.insert(0, "/dev-work")
import w8a16_tl as W
from bench_gemm import SHAPES, make, graph_time

dev = "cuda"
sms = torch.cuda.get_device_properties(0).multi_processor_count
for name in ["kda_qkv", "kda_o", "mla_qkv_a", "mla_q_b", "mla_o", "shexp_down"]:
    N, K, _ = SHAPES[name]
    ncopy = max(1, -(-384 * 2**20 // (N * K)))
    ws = [make(N, K, dev) for _ in range(ncopy)]
    tiles, kb, split = N // 32, K // 128, 1
    while tiles * split < 2 * sms and kb % (split * 2) == 0 and split < 16:
        split *= 2
    M = 6
    x0 = torch.randn(M, K, device=dev, dtype=torch.bfloat16)
    x = torch.empty_like(x0)
    big = torch.randn(4 * 1024 * 1024, device=dev)  # producer: ~16MB elementwise (a few us)
    res = {}
    for ver, builder in [("v2", W.build_v2), ("v3pdl", W.build_v3)]:
        k = builder(N, K, 16, 32, 128, split, 128, 2)
        P = torch.empty(split, 16, N, dtype=torch.float32, device=dev)
        cnt = torch.zeros(N // 32, dtype=torch.int32, device=dev)
        out = torch.empty(M, N, dtype=torch.bfloat16, device=dev)
        it = [0]

        def step():
            w, s = ws[it[0] % ncopy]
            it[0] += 1
            big.mul_(1.0)            # producer kernel
            torch.mul(x0, 1.0, out=x)  # writes A right before the GEMM
            k(x, w, s, out, P, cnt)

        def producer_only():
            big.mul_(1.0)
            torch.mul(x0, 1.0, out=x)

        t = graph_time(step, iters=max(20, ncopy))
        t0 = graph_time(producer_only, iters=max(20, ncopy))
        ref = x.float() @ (ws[(it[0] - 1) % ncopy][0].float() * ws[(it[0] - 1) % ncopy][1].repeat_interleave(128, 0).repeat_interleave(128, 1)).T
        k(x, *ws[(it[0] - 1) % ncopy], out, P, cnt)
        err = ((out.float() - ref).abs().max() / ref.abs().max()).item()
        res[ver] = (t - t0, err)
    print(f"{name:12s} split={split:2d} gemm-time-in-chain v2={res['v2'][0]:6.1f}us v3pdl={res['v3pdl'][0]:6.1f}us err={res['v3pdl'][1]:.1e}", flush=True)
