"""All-reduce microbench at GLM-5.3 prefill/decode message sizes (torchrun, 2 ranks)."""
import os, time, torch, torch.distributed as dist
dist.init_process_group("nccl")
r = dist.get_rank(); torch.cuda.set_device(r)
for tokens in [6, 24, 128, 1024, 6144]:
    x = torch.randn(tokens, 4096, dtype=torch.bfloat16, device="cuda")
    for _ in range(5): dist.all_reduce(x)
    torch.cuda.synchronize()
    n = 20 if tokens >= 1024 else 200
    t = time.perf_counter()
    for _ in range(n): dist.all_reduce(x)
    torch.cuda.synchronize()
    dt = (time.perf_counter() - t) / n
    if r == 0:
        mb = x.numel() * 2 / 1e6
        print(f"PROTO={os.environ.get('NCCL_PROTO','default'):7s} tokens={tokens:5d} {mb:7.2f}MB {dt*1e6:9.1f}us algbw={mb/1e3/dt:6.1f}GB/s", flush=True)
dist.destroy_process_group()
