"""Tuned FlashInfer IPC all-reduce of a 6144-token tensor via row chunks."""
import os, time, torch, torch.distributed as dist
from flashinfer.comm import PcieIpcAllReduceWorkspace
dist.init_process_group("nccl")
r = dist.get_rank(); torch.cuda.set_device(r)
rows = int(os.environ["ROWS"])
free0 = torch.cuda.mem_get_info()[0]
ws = PcieIpcAllReduceWorkspace(group=dist.group.WORLD, max_numel=rows * 4096, dtype=torch.bfloat16)
torch.cuda.synchronize()
mib = (free0 - torch.cuda.mem_get_info()[0]) / 2**20
ws.tune([4096], tune_group=dist.new_group(backend="gloo"))
def chunked(x):
    out = torch.empty_like(x)
    for r0 in range(0, x.shape[0], rows):
        ws.all_reduce(x[r0:r0 + rows], out=out[r0:r0 + rows])
    return out
for tokens in [1024, 2048, 4096, 6144]:
    x = torch.randn(tokens, 4096, dtype=torch.bfloat16, device="cuda") / 8
    ref = x.clone(); dist.all_reduce(ref)
    err = (chunked(x).float() - ref.float()).abs().max().item()
    for _ in range(5): chunked(x)
    torch.cuda.synchronize(); dist.barrier()
    t = time.perf_counter()
    for _ in range(20): chunked(x)
    torch.cuda.synchronize()
    if r == 0:
        print(f"rows/chunk={rows:5d} ws_MiB~{mib:6.0f} tokens={tokens:5d} {1e6*(time.perf_counter()-t)/20:8.1f}us maxerr={err}", flush=True)
dist.destroy_process_group()
