"""TileLang W8A16 block-FP8 split-K GEMM for small-M decode on SM120.

out[M, N] = A[M, K] (bf16) @ dequant(B[N, K] fp8_e4m3, S[N/128, K/128] fp32)^T

The checkpoint's FP8_PB_WO layers are *weight-only* FP8: activations stay in
BF16. The weight tile is converted FP8->BF16 exactly (every e4m3 value is a
bf16 value); each 128x128 block scale is applied to the fp32 per-k-block
partial product, so no rounding is introduced beyond BF16 MMA accumulation.

Small M cannot fill 188 SMs with N/BN tiles, so K is split across `split`
CTAs per N tile. Partials go to an fp32 workspace and the last CTA to finish a
tile (device-scope atomic ticket) sums them in a fixed order and writes BF16.
That keeps the result deterministic and needs no second kernel launch; the
ticket is reset by the reducing CTA so CUDA-graph replays start from zero.
"""
import functools

import tilelang
import tilelang.language as T
import torch

PASS_CFG = {
    tilelang.PassConfigKey.TL_DISABLE_WARP_SPECIALIZED: True,
    tilelang.PassConfigKey.TL_DISABLE_TMA_LOWER: True,
}


@functools.lru_cache(maxsize=None)
def build(N, K, BM=16, BN=64, BK=128, split=4, threads=128, stages=3, rs=False, v2=False):
    if v2:
        return build_v2(N, K, BM, BN, BK, split, threads, stages)
    if rs:
        return build_rs(N, K, BM, BN, BK, split, threads, stages)
    assert K % BK == 0 and N % BN == 0 and 128 % BN == 0 or BN % 128 == 0
    KB = K // BK
    assert KB % split == 0, (K, BK, split)
    kps = KB // split
    NT = N // BN
    SN = (N + 127) // 128
    SK = K // 128
    M = T.dynamic("m")

    @tilelang.jit(pass_configs=PASS_CFG)
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
                part = T.alloc_fragment((BM, BN), "float32")
                red = T.alloc_fragment((BM, BN), "float32")
                tmp = T.alloc_fragment((BM, BN), "float32")
                ticket = T.alloc_shared((1,), "int32")
                T.clear(acc)
                for ko in T.Pipelined(kps, num_stages=stages):
                    kb = bz * kps + ko
                    T.copy(A[0, kb * BK], A_s)
                    T.copy(B[bx * BN, kb * BK], B_s)
                    T.copy(B_s, B_h)
                    T.gemm(A_s, B_h, part, transpose_B=True, clear_accum=True)
                    sc = S[(bx * BN) // 128, (kb * BK) // 128]
                    for i, j in T.Parallel(BM, BN):
                        acc[i, j] += part[i, j] * sc
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
                            T.copy(P[s, 0, bx * BN], tmp)
                            for i, j in T.Parallel(BM, BN):
                                red[i, j] += tmp[i, j]
                        T.copy(red, C[0, bx * BN])
                        if tx == 0:
                            Cnt[bx] = 0

        return main

    return kern()


class W8A16:
    """Holds per-shape workspace and counters; call(x, w, s) -> bf16."""

    def __init__(self, N, K, device, **cfg):
        self.cfg = dict(cfg)
        self.N, self.K = N, K
        self.kernel = build(N, K, **cfg)
        BM = cfg.get("BM", 16)
        split = cfg.get("split", 4)
        BN = cfg.get("BN", 64)
        self.ws = torch.empty(split, BM, N, dtype=torch.float32, device=device)
        self.cnt = torch.zeros(N // BN, dtype=torch.int32, device=device)
        self.BM = BM

    def __call__(self, x, w, s):
        assert x.shape[0] <= self.BM
        out = torch.empty(x.shape[0], self.N, dtype=torch.bfloat16, device=x.device)
        self.kernel(x, w, s, out, self.ws, self.cnt)
        return out


@functools.lru_cache(maxsize=None)
def build_rs(N, K, BM, BN, BK, split, threads, stages):
    """Transposed formulation: acc[BN, BM] = W_tile[BN, BK] @ A_tile[BM, BK]^T.

    The FP8 weight tile is converted into a BF16 register fragment (MMA "A"
    operand), so shared memory holds only the cp.async FP8 stages and the tiny
    activation tile; the token dimension becomes the MMA N (multiple of 8).
    """
    KB = K // BK
    assert KB % split == 0, (K, BK, split)
    kps = KB // split
    NT = N // BN
    SN = (N + 127) // 128
    SK = K // 128
    M = T.dynamic("m")

    @tilelang.jit(pass_configs=PASS_CFG)
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
                B_r = T.alloc_fragment((BN, BK), "bfloat16")
                acc = T.alloc_fragment((BN, BM), "float32")
                part = T.alloc_fragment((BN, BM), "float32")
                red = T.alloc_fragment((BN, BM), "float32")
                tmp = T.alloc_fragment((BN, BM), "float32")
                ticket = T.alloc_shared((1,), "int32")
                T.clear(acc)
                for ko in T.Pipelined(kps, num_stages=stages):
                    kb = bz * kps + ko
                    T.copy(A[0, kb * BK], A_s)
                    T.copy(B[bx * BN, kb * BK], B_s)
                    T.copy(B_s, B_r)
                    T.gemm(B_r, A_s, part, transpose_B=True, clear_accum=True)
                    sc = S[(bx * BN) // 128, (kb * BK) // 128]
                    for i, j in T.Parallel(BN, BM):
                        acc[i, j] += part[i, j] * sc
                if split == 1:
                    for i, j in T.Parallel(BN, BM):
                        if j < M:
                            C[j, bx * BN + i] = acc[i, j]
                else:
                    for i, j in T.Parallel(BN, BM):
                        P[bz, j, bx * BN + i] = acc[i, j]
                    T.sync_threads()
                    tx = T.get_thread_binding()
                    if tx == 0:
                        ticket[0] = T.atomic_add(Cnt[bx], 1, memory_order="acq_rel", return_prev=True)
                    T.sync_threads()
                    if ticket[0] == split - 1:
                        T.clear(red)
                        for s in T.serial(split):
                            for i, j in T.Parallel(BN, BM):
                                red[i, j] += P[s, j, bx * BN + i]
                        for i, j in T.Parallel(BN, BM):
                            if j < M:
                                C[j, bx * BN + i] = red[i, j]
                        if tx == 0:
                            Cnt[bx] = 0

        return main

    return kern()


@functools.lru_cache(maxsize=None)
def build_v2(N, K, BM, BN, BK, split, threads, stages):
    """Scale folded into the FP8->BF16 conversion; one MMA chain per k-step.

    B_h[n, k] = bf16(fp8(B[n, k]) * S[n // 128, k // 128]). Rounding the
    scaled weight to BF16 is the standard W8A16 dequant (relative 2^-9 per
    element, uncorrelated across the dot product); it allows BK > 128.
    """
    assert BN <= 128 and 128 % BN == 0
    KB = K // BK
    assert K % BK == 0 and KB % split == 0, (K, BK, split)
    kps = KB // split
    NT = N // BN
    SN = (N + 127) // 128
    SK = K // 128
    M = T.dynamic("m")

    @tilelang.jit(pass_configs=PASS_CFG)
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


PREFETCH_SRC = r"""
__device__ __forceinline__ void kexp_prefetch_l2(const void* p) {
  asm volatile("prefetch.global.L2 [%0];" :: "l"(p));
}
"""


@functools.lru_cache(maxsize=None)
def build_v3(N, K, BM, BN, BK, split, threads, stages):
    """v2 + PDL: prefetch this CTA's weight slab into L2, then griddepcontrol.wait.

    Weights never depend on the producer kernel, so their DRAM stream can start
    while the producer drains; only the activation loads wait for it.
    """
    assert BN <= 128 and 128 % BN == 0
    KB = K // BK
    assert K % BK == 0 and KB % split == 0, (K, BK, split)
    kps = KB // split
    NT = N // BN
    SN = (N + 127) // 128
    SK = K // 128
    lines = kps * BK // 128  # 128-byte lines per weight row in this CTA's slab
    n_pf = BN * lines
    M = T.dynamic("m")

    @tilelang.jit(pass_configs=PASS_CFG)
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
                T.import_source(PREFETCH_SRC)
                A_s = T.alloc_shared((BM, BK), "bfloat16")
                B_s = T.alloc_shared((BN, BK), "float8_e4m3")
                B_h = T.alloc_shared((BN, BK), "bfloat16")
                acc = T.alloc_fragment((BM, BN), "float32")
                red = T.alloc_fragment((BM, BN), "float32")
                ticket = T.alloc_shared((1,), "int32")
                tx = T.get_thread_binding()
                for t in T.serial((n_pf + threads - 1) // threads):
                    idx = t * threads + tx
                    if idx < n_pf:
                        T.call_extern(
                            "handle",
                            "kexp_prefetch_l2",
                            T.address_of(B[bx * BN + idx // lines, bz * kps * BK + (idx % lines) * 128]),
                        )
                T.pdl_sync()
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
