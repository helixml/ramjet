"""Make SGLang's NUMA bind soft on memory: --preferred=N instead of --membind=N.
CPU binding is unchanged. A rank's large host allocations (DSv4.1's ~190GB shared
Engram table) can then spill to other NUMA nodes instead of being OOM-killed under
a single-node memory policy. Opt-in: SGLANG_NUMA_MEM_PREFERRED=1."""
import pathlib, sys
root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/sgl-workspace/sglang/python/sglang")
p = root / "srt/utils/numa_utils.py"; s = p.read_text()
if "_numactl_cpu_mem_args_hard" in s:  # a container restart re-runs the start command
    print("already patched srt/utils/numa_utils.py"); raise SystemExit(0)
old = "def _numactl_cpu_mem_args(node: int, gpu_id: int) -> Optional[str]:\n"
if s.count(old) != 1:
    raise SystemExit("anchor not found in numa_utils.py")
s = s.replace(old, '''def _numactl_cpu_mem_args(node: int, gpu_id: int) -> Optional[str]:
    args = _numactl_cpu_mem_args_hard(node, gpu_id)
    import os as _os
    if args and _os.environ.get("SGLANG_NUMA_MEM_PREFERRED", "0") == "1":
        args = args.replace("--membind=", "--preferred=")
    return args


def _numactl_cpu_mem_args_hard(node: int, gpu_id: int) -> Optional[str]:
''')
p.write_text(s); print("patched srt/utils/numa_utils.py")
