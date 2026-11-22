"""TP4 all-reduce at GLM-5.3 sizes: NCCL (env-selected) vs FlashInfer PCIe IPC (untuned/tuned/chunked)."""
import os, time, torch, torch.distributed as dist
from flashinfer.comm import PcieIpcAllReduceWorkspace
dist.init_process_group("nccl")
r = dist.get_rank(); torch.cuda.set_device(r % torch.cuda.device_count())
W = 786432  # production SGLANG_PCIE_IPC_MAX_NUMEL (192 x 4096)
ws = PcieIpcAllReduceWorkspace(group=dist.group.WORLD, max_numel=W, dtype=torch.bfloat16)
rows = W // 4096
def chunked(x):
    if ws.supports(x): return ws.all_reduce(x)
    out = torch.empty_like(x)
    for r0 in range(0, x.shape[0], rows):
        ws.all_reduce(x[r0:r0 + rows], out=out[r0:r0 + rows])
    return out
def bench(fn, x, n):
    for _ in range(5): fn(x)
    torch.cuda.synchronize(); dist.barrier()
    ts = []
    for _ in range(3):
        t = time.perf_counter()
        for _ in range(n): fn(x)
        torch.cuda.synchronize(); ts.append((time.perf_counter() - t) / n)
    return sorted(ts)[1]
sizes = [1, 6, 24, 192, 1024, 6144]
res = {}
for label in ["untuned", "tuned"]:
    if label == "tuned":
        ws.tune([4096], tune_group=dist.new_group(backend="gloo"))
    for tokens in sizes:
        x = torch.randn(tokens, 4096, dtype=torch.bfloat16, device="cuda") / 8
        ref = x.clone(); dist.all_reduce(ref)
        err = (chunked(x.clone()).float() - ref.float()).abs().max().item()
        n = 20 if tokens >= 1024 else 200
        res[(label, tokens)] = (bench(chunked, x, n), err)
        if label == "untuned":
            res[("nccl", tokens)] = (bench(lambda y: dist.all_reduce(y), x.clone(), n), 0.0)
if r == 0:
    tag = " ".join(f"{k}={os.environ[k]}" for k in sorted(os.environ) if k.startswith("NCCL_")) or "default"
    for tokens in sizes:
        print(f"[{tag}] tokens={tokens:5d} nccl={res[('nccl',tokens)][0]*1e6:8.1f}us "
              f"ipc_untuned={res[('untuned',tokens)][0]*1e6:8.1f}us ipc_tuned={res[('tuned',tokens)][0]*1e6:8.1f}us "
              f"maxerr={res[('tuned',tokens)][1]:.3g}", flush=True)
dist.destroy_process_group()
