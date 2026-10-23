"""Diagnostic: time large TP all-reduces on the engine's tensor vs a fresh copy.

Both ranks issue the same reductions in the same order, so the extra
(discarded) all-reduce on the copy is collective-safe. Only the first few
6144x4096 calls per process are instrumented.
"""
import os
import sys
import time

if os.environ.get("KEXP_AR_DIAG") == "1":
    import torch
    import torch.distributed as dist

    _orig = dist.all_reduce
    _state = {"n": 0}
    TARGET = int(os.environ.get("KEXP_AR_DIAG_NUMEL", str(6144 * 4096)))

    def _timed(fn):
        torch.cuda.synchronize()
        t = time.perf_counter()
        r = fn()
        torch.cuda.synchronize()
        return r, (time.perf_counter() - t) * 1e6

    def all_reduce(tensor, *args, **kwargs):
        if (
            _state["n"] >= 6
            or not isinstance(tensor, torch.Tensor)
            or tensor.numel() != TARGET
            or not tensor.is_cuda
            or torch.cuda.is_current_stream_capturing()
            or kwargs.get("async_op")
        ):
            return _orig(tensor, *args, **kwargs)
        _state["n"] += 1
        fresh = torch.empty(tensor.shape, dtype=tensor.dtype, device=tensor.device)
        fresh.copy_(tensor)
        _, t_fresh_before = _timed(lambda: _orig(fresh, *args, **kwargs))
        r, t_engine = _timed(lambda: _orig(tensor, *args, **kwargs))
        _, t_fresh_after = _timed(lambda: _orig(fresh, *args, **kwargs))
        st = tensor.untyped_storage()
        print(
            f"[kexp-ar-diag pid={os.getpid()} call={_state['n']}] engine_tensor={t_engine:.0f}us "
            f"fresh_copy_before={t_fresh_before:.0f}us fresh_copy_after={t_fresh_after:.0f}us "
            f"shape={tuple(tensor.shape)} stride={tensor.stride()} contig={tensor.is_contiguous()} "
            f"offset={tensor.storage_offset()} storage_bytes={st.nbytes()} "
            f"ptr_mod_4k={tensor.data_ptr() % 4096} stream={torch.cuda.current_stream().cuda_stream}",
            file=sys.stderr,
            flush=True,
        )
        return r

    dist.all_reduce = all_reduce
