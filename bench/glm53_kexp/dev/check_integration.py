"""Patched fp8_utils: eager vs reference, graph replay vs eager, fallbacks."""
import logging
import torch
logging.basicConfig(level=logging.INFO)
from sglang.srt.layers.quantization import fp8_utils, w8a16_tilelang as tl
assert tl.enabled()
dev = "cuda"
for (N, K) in [(4096, 4096), (2048, 4096), (12288, 4096), (4096, 1024), (8192, 1536)]:
    w = (torch.randn(N, K, device=dev) * 0.5).to(torch.float8_e4m3fn)
    s = torch.rand(N // 128, K // 128, device=dev) * 0.02 + 0.001
    sf = s.repeat_interleave(128, 0).repeat_interleave(128, 1)
    for M in [1, 4, 6, 17, 24, 64]:
        x = torch.randn(M, K, device=dev, dtype=torch.bfloat16)
        ref = x.float() @ (w.float() * sf).T
        y = fp8_utils.cutlass_w8a8_block_fp8_linear_with_fallback(x, w, [128, 128], s)
        rel = ((y.float() - ref).abs().max() / ref.abs().max()).item()
        used = tl.supports(x, w, s, [128, 128])
        # graph replay must equal eager bit-for-bit (deterministic reduction)
        xs = x.clone()
        g = torch.cuda.CUDAGraph()
        st = torch.cuda.Stream(); st.wait_stream(torch.cuda.current_stream())
        with torch.cuda.stream(st):
            fp8_utils.cutlass_w8a8_block_fp8_linear_with_fallback(xs, w, [128, 128], s)
        torch.cuda.current_stream().wait_stream(st)
        with torch.cuda.graph(g):
            yg = fp8_utils.cutlass_w8a8_block_fp8_linear_with_fallback(xs, w, [128, 128], s)
        same = True
        for _ in range(5):
            g.replay()
            torch.cuda.synchronize()
            same &= torch.equal(yg, y)
        print(f"N={N} K={K} M={M:3d} tilelang={used} rel_err={rel:.2e} graph_replay_equal={same}")
        assert rel < (0.01 if used else 0.06), rel
        assert same or not used
print("OK")
