"""TileLang W8A16 block-FP8 split-K GEMM for small-M (decode) linears on SM120.

ModelOpt ``FP8_PB_WO`` checkpoints are FP8 *weight-only* (128x128 block
scales, BF16 activations). SGLang serves them through the W8A8 block-FP8
path, which quantizes every activation row to FP8 and runs a CUTLASS
128-wide tile GEMM. At decode M (1-32 tokens) that GEMM launches N/128 CTAs
on a 188-SM GPU, so the memory system is under-occupied, and the activation
quantization adds a kernel plus ~10x more error than the checkpoint carries.

This kernel keeps activations in BF16, converts each FP8 weight tile to BF16
with its block scale folded in, runs mma.sync BF16 MMAs, and splits K across
CTAs so enough loads are in flight. The last CTA to finish an N tile (device
atomic ticket) sums the fp32 partials in fixed order: deterministic, no second
launch, and the ticket resets itself so CUDA-graph replays start from zero.

Enabled with SGLANG_W8A16_TILELANG=1; anything outside the admitted shape
window falls back to the caller's existing path.
"""
# No `from __future__ import annotations`: TileLang evaluates the prim_func
# signature annotations (which close over the dynamic M) at decoration time.
import functools
import logging
import os
import threading
from typing import Dict, Optional, Tuple

import torch

logger = logging.getLogger(__name__)

_ENABLED = os.environ.get("SGLANG_W8A16_TILELANG", "0") == "1"
# Largest weight (N*K elements) routed here. The 317M-element FP8 lm_head is
# long enough for CUTLASS's 128-wide tiles to saturate DRAM, so it stays there.
_MAX_NK = int(os.environ.get("SGLANG_W8A16_TILELANG_MAX_NK", str(64 * 1024 * 1024)))
_MAX_M = 32
_BN = 32
_BK = 128
_THREADS = 128
_SMS = None
_lock = threading.Lock()
_workspaces: Dict[Tuple[int, int, int, int, int], Tuple[torch.Tensor, torch.Tensor]] = {}


_tls = threading.local()


def enabled() -> bool:
    return _ENABLED


class disabled:
    """Route GEMMs issued inside this block back to the caller's path."""

    def __enter__(self):
        _tls.depth = getattr(_tls, "depth", 0) + 1

    def __exit__(self, *exc):
        _tls.depth -= 1
        return False


def _num_sms(device) -> int:
    global _SMS
    if _SMS is None:
        _SMS = torch.cuda.get_device_properties(device).multi_processor_count
    return _SMS


def _config(N: int, K: int, device) -> Tuple[int, int]:
    """(split, stages) giving roughly two CTAs per SM (measured optimum)."""
    tiles = N // _BN
    kb = K // _BK
    split = 1
    target = 2 * _num_sms(device)
    while tiles * split < target and kb % (split * 2) == 0 and split < 16:
        split *= 2
    return split, 2


def supports(x: torch.Tensor, weight: torch.Tensor, weight_scale: torch.Tensor, block_size) -> bool:
    if not _ENABLED or getattr(_tls, "depth", 0):
        return False
    if x.dtype != torch.bfloat16 or weight.dtype != torch.float8_e4m3fn:
        return False
    if list(block_size) != [128, 128] or weight_scale.dtype != torch.float32:
        return False
    N, K = weight.shape
    if N % _BN or K % _BK or N * K > _MAX_NK:
        return False
    M = x.numel() // x.shape[-1]
    if not (0 < M <= _MAX_M and x.shape[-1] == K):
        return False
    # Long weight streams (KDA qkv, 50M elements) only win at M <= 16.
    if N * K > 34 * 1024 * 1024 and M > 16:
        return False
    if weight_scale.shape != ((N + 127) // 128, K // 128) or not weight_scale.is_contiguous():
        return False
    return weight.is_contiguous()


@functools.lru_cache(maxsize=None)
def _build(N: int, K: int, BM: int, split: int, stages: int):
    import tilelang
    import tilelang.language as T

    BN, BK, threads = _BN, _BK, _THREADS
    KB = K // BK
    kps = KB // split
    NT = N // BN
    SN = (N + 127) // 128
    SK = K // 128
    M = T.dynamic("m")
    pass_cfg = {
        tilelang.PassConfigKey.TL_DISABLE_WARP_SPECIALIZED: True,
        tilelang.PassConfigKey.TL_DISABLE_TMA_LOWER: True,
    }

    @tilelang.jit(pass_configs=pass_cfg)
    def kern():
        @T.prim_func
        def main(
            A: T.Tensor((M, K), "bfloat16"),
            B: T.Tensor((N, K), "float8_e4m3"),
            S: T.Tensor((SN, SK), "float32"),
            C: T.Tensor((M, N), "bfloat16"),
            P: T.Tensor((split, BM, N), "float32"),
            Cnt: T.Tensor((NT,), "int32"),
        ):
            with T.Kernel(NT, split, threads=threads) as (bx, bz):
                A_s = T.alloc_shared((BM, BK), "bfloat16")
                B_s = T.alloc_shared((BN, BK), "float8_e4m3")
                B_h = T.alloc_shared((BN, BK), "bfloat16")
                acc = T.alloc_fragment((BM, BN), "float32")
                red = T.alloc_fragment((BM, BN), "float32")
                ticket = T.alloc_shared((1,), "int32")
                nb = (bx * BN) // 128
                T.clear(acc)
                for ko in T.Pipelined(kps, num_stages=stages):
                    kb = bz * kps + ko
                    T.copy(A[0, kb * BK], A_s)
                    T.copy(B[bx * BN, kb * BK], B_s)
                    for i, k in T.Parallel(BN, BK):
                        B_h[i, k] = B_s[i, k] * S[nb, (kb * BK + k) // 128]
                    T.gemm(A_s, B_h, acc, transpose_B=True)
                if split == 1:
                    T.copy(acc, C[0, bx * BN])
                else:
                    T.copy(acc, P[bz, 0, bx * BN])
                    T.sync_threads()
                    tx = T.get_thread_binding()
                    if tx == 0:
                        ticket[0] = T.atomic_add(Cnt[bx], 1, memory_order="acq_rel", return_prev=True)
                    T.sync_threads()
                    if ticket[0] == split - 1:
                        T.clear(red)
                        for s in T.serial(split):
                            for i, j in T.Parallel(BM, BN):
                                red[i, j] += P[s, i, bx * BN + j]
                        T.copy(red, C[0, bx * BN])
                        if tx == 0:
                            Cnt[bx] = 0

        return main

    return kern()


def _workspace(N, K, BM, split, device) -> Tuple[torch.Tensor, torch.Tensor]:
    key = (N, K, BM, split, device.index if device.index is not None else torch.cuda.current_device())
    ws = _workspaces.get(key)
    if ws is None:
        with _lock:
            ws = _workspaces.get(key)
            if ws is None:
                # Allocated outside any CUDA-graph pool on first (warmup) use;
                # same-shape GEMMs run back to back on one stream, so sharing
                # the partial buffer and self-resetting tickets is safe.
                P = torch.empty(split, BM, N, dtype=torch.float32, device=device)
                cnt = torch.zeros(N // _BN, dtype=torch.int32, device=device)
                ws = (P, cnt)
                _workspaces[key] = ws
    return ws


_checked = set()
_CHECK = os.environ.get("SGLANG_W8A16_TILELANG_CHECK", "1") == "1"


def maybe_check(out, x, weight, weight_scale, reference_fn):
    """Once per (N, K, BM): compare against the incumbent path and log it."""
    if not _CHECK or torch.cuda.is_current_stream_capturing():
        return
    N, K = weight.shape
    M = x.numel() // K
    key = (N, K, 16 if M <= 16 else 32)
    if key in _checked:
        return
    _checked.add(key)
    ref = reference_fn().float().reshape(-1, N)
    o = out.float().reshape(-1, N)
    rel = ((o - ref).abs().max() / ref.abs().max().clamp_min(1e-6)).item()
    logger.info(f"W8A16 TileLang check N={N} K={K} M={M}: max rel diff vs W8A8 CUTLASS {rel:.3e}")
    if not rel < 0.1:
        raise RuntimeError(f"W8A16 TileLang mismatch N={N} K={K} M={M}: rel {rel}")


def w8a16_block_fp8_linear(
    x: torch.Tensor,
    weight: torch.Tensor,
    weight_scale: torch.Tensor,
    bias: Optional[torch.Tensor] = None,
) -> torch.Tensor:
    N, K = weight.shape
    x2 = x.reshape(-1, K)
    if not x2.is_contiguous():
        x2 = x2.contiguous()
    M = x2.shape[0]
    BM = 16 if M <= 16 else 32
    split, stages = _config(N, K, x.device)
    kernel = _build(N, K, BM, split, stages)
    P, cnt = _workspace(N, K, BM, split, x.device)
    out = torch.empty(M, N, dtype=torch.bfloat16, device=x.device)
    kernel(x2, weight, weight_scale, out, P, cnt)
    if bias is not None:
        out += bias
    return out.view(*x.shape[:-1], N)
