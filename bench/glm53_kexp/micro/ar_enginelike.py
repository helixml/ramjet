"""All-reduce through an SGLang-shaped TP subgroup (world PG lazy, then new_group)."""
import os, time, torch, torch.distributed as dist
dist.init_process_group(backend="nccl")
r = dist.get_rank(); torch.cuda.set_device(r)
kw = {}
if os.environ.get("LOCAL_SYNC") is not None:
    kw["use_local_synchronization"] = os.environ["LOCAL_SYNC"] == "1"
tp = dist.new_group(ranks=[0, 1], backend="nccl", **kw)
cpu = dist.new_group(ranks=[0, 1], backend="gloo", **kw)
which = os.environ.get("GROUP", "tp")
g = tp if which == "tp" else None
x = torch.randn(6144, 4096, dtype=torch.bfloat16, device="cuda")
for _ in range(5): dist.all_reduce(x, group=g)
torch.cuda.synchronize(); dist.barrier(group=cpu)
t = time.perf_counter()
for _ in range(20): dist.all_reduce(x, group=g)
torch.cuda.synchronize()
def shm_numa():
    n = {}
    for line in open("/proc/self/numa_maps"):
        if "nccl-" in line:
            for tok in line.split():
                if tok.startswith("N") and "=" in tok:
                    k, v = tok.split("=")
                    n[k] = n.get(k, 0) + int(v)
    return n
pl = [None, None]
dist.all_gather_object(pl, shm_numa(), group=cpu)
if r == 0:
    print(f"shm_pages_by_node rank0={pl[0]} rank1={pl[1]}", flush=True)
    print(f"group={which} local_sync={kw.get('use_local_synchronization')} 6144x4096: {(time.perf_counter()-t)/20*1e6:.0f}us", flush=True)
dist.destroy_process_group()
