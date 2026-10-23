"""FlashInfer PcieIpcAllReduceWorkspace vs NCCL at GLM-5.3 TP2 message sizes."""
import os, time, torch, torch.distributed as dist
from flashinfer.comm import PcieIpcAllReduceWorkspace
dist.init_process_group("nccl")
r = dist.get_rank(); torch.cuda.set_device(r)
max_numel = int(os.environ.get("MAX_NUMEL", 6144 * 4096))
free0 = torch.cuda.mem_get_info()[0]
ws = PcieIpcAllReduceWorkspace(group=dist.group.WORLD, max_numel=max_numel, dtype=torch.bfloat16)
torch.cuda.synchronize()
if r == 0: print("profile", ws.profile, "workspace_MiB", (free0 - torch.cuda.mem_get_info()[0]) / 2**20, flush=True)
import inspect
if os.environ.get("TUNE") == "1":
    g = dist.new_group(backend="gloo")
    ws.tune([4096], tune_group=g)
    if r == 0: print("tuned", flush=True)
def bench(fn, x, n):
    for _ in range(5): fn(x)
    torch.cuda.synchronize(); dist.barrier()
    t = time.perf_counter()
    for _ in range(n): fn(x)
    torch.cuda.synchronize()
    return (time.perf_counter() - t) / n
for tokens in [6, 24, 192, 1024, 2048, 6144]:
    x = torch.randn(tokens, 4096, dtype=torch.bfloat16, device="cuda") / 8
    ref = x.clone(); dist.all_reduce(ref)
    got = ws.all_reduce(x.clone())
    err = (got.float() - ref.float()).abs().max().item()
    n = 20 if tokens >= 1024 else 200
    t_ipc = bench(ws.all_reduce, x, n)
    t_nccl = bench(lambda y: dist.all_reduce(y), x.clone(), n)
    if r == 0:
        print(f"tokens={tokens:5d} ipc={t_ipc*1e6:8.1f}us nccl={t_nccl*1e6:8.1f}us maxerr={err:.3g} supports={ws.supports(x)}", flush=True)
dist.destroy_process_group()
