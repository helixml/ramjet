import torch, time
n = 50_331_648  # bytes (6144 x 4096 bf16)
a0 = torch.empty(n, dtype=torch.uint8, device="cuda:0"); b0 = torch.empty(n, dtype=torch.uint8, device="cuda:0")
a1 = torch.empty(n, dtype=torch.uint8, device="cuda:1"); b1 = torch.empty(n, dtype=torch.uint8, device="cuda:1")
print("can_access_peer", torch.cuda.can_device_access_peer(0, 1))
s0 = torch.cuda.Stream(device=0); s1 = torch.cuda.Stream(device=1)
def run(bidir, iters=20):
    for _ in range(3):
        with torch.cuda.stream(s0): b1.copy_(a0, non_blocking=True)
        if bidir:
            with torch.cuda.stream(s1): b0.copy_(a1, non_blocking=True)
    torch.cuda.synchronize(0); torch.cuda.synchronize(1)
    t = time.perf_counter()
    for _ in range(iters):
        with torch.cuda.stream(s0): b1.copy_(a0, non_blocking=True)
        if bidir:
            with torch.cuda.stream(s1): b0.copy_(a1, non_blocking=True)
    torch.cuda.synchronize(0); torch.cuda.synchronize(1)
    dt = (time.perf_counter() - t) / iters
    print(f"bidir={bidir} {dt*1e6:.0f}us per-direction {n/dt/1e9:.1f} GB/s")
run(False); run(True)
