#!/usr/bin/env python3
"""Serve prefill-sized TP all-reduces through the tuned FlashInfer PCIe IPC path.

node06's GLM pairs meet only at a PCIe host bridge (``nvidia-smi topo``:
NODE), so NCCL stages every all-reduce through host memory (SHM transport).
SGLang's FlashInfer PCIe IPC all-reduce is peer-to-peer, but it only admits
tensors up to ``SGLANG_PCIE_IPC_MAX_NUMEL`` (192 tokens x 4096) and is never
tuned, so every prefill chunk falls back to NCCL. A 6,144 x 4,096 BF16
all-reduce measured 2.30 ms on NCCL SHM, 1.75 ms on NCCL P2P, and 1.46 ms as
tuned 192-row chunks through the existing workspace (bit-identical results,
no extra device memory). End to end, cold prefill gained 9-10%.

With ``SGLANG_PCIE_IPC_CHUNKED=1``, tensors larger than the workspace are
served as contiguous row chunks; admission stays rank-invariant (shape,
dtype, contiguity only). Pair it with ``SGLANG_PCIE_IPC_TUNE=1``.
"""

from __future__ import annotations

import hashlib
from pathlib import Path


SRC = Path("/opt/sglang-source/python/sglang/srt")
PARALLEL_STATE = SRC / "distributed/parallel_state.py"
PARALLEL_STATE_SHA256 = "43de999ff543dbced6982763a41ea202c48b53dbf10cd9df603d52aee232e5b2"
ENVIRON = SRC / "environ.py"
ENVIRON_SHA256 = "6f3114f1ba5a078e2a3ab42a473362a92f41fb0f42f9a1cf3f1d84ea8828bf47"


def replace_once(source: str, old: str, new: str, what: str) -> str:
    if source.count(old) != 1:
        raise SystemExit(f"pinned source shape changed: {what}")
    return source.replace(old, new, 1)


def patch_parallel_state(source: str) -> str:
    source = replace_once(
        source,
        """        ws = self.pcie_ipc_ws
        if ws is None or input_.dim() != 2:
            return False
        return ws.supports(input_)
""",
        """        ws = self.pcie_ipc_ws
        if ws is None or input_.dim() != 2:
            return False
        if ws.supports(input_):
            return True
        # Prefill-sized tensors exceed the workspace; row chunks of a
        # contiguous tensor are contiguous, so serve them chunk by chunk.
        rows = self._pcie_ipc_chunk_rows(input_)
        return rows > 0 and input_.is_contiguous() and ws.supports(input_[:rows])

    def _pcie_ipc_chunk_rows(self, input_: torch.Tensor) -> int:
        if not envs.SGLANG_PCIE_IPC_CHUNKED.get():
            return 0
        return envs.SGLANG_PCIE_IPC_MAX_NUMEL.get() // input_.shape[1]
""",
        "PCIe IPC admission",
    )
    return replace_once(
        source,
        """                ws.rebind_stream()
                self._pcie_ipc_stream = current
        return ws.all_reduce(input_)
""",
        """                ws.rebind_stream()
                self._pcie_ipc_stream = current
        if ws.supports(input_):
            return ws.all_reduce(input_)
        # Tuned chunks beat NCCL at every size on PCIe-only TP2 (6144 tokens:
        # ~1.46 ms vs ~2.3 ms SHM / ~1.75 ms P2P), with no larger workspace.
        rows = self._pcie_ipc_chunk_rows(input_)
        out = torch.empty_like(input_)
        for r0 in range(0, input_.shape[0], rows):
            ws.all_reduce(input_[r0 : r0 + rows], out=out[r0 : r0 + rows])
        return out
""",
        "PCIe IPC all-reduce",
    )


def patch_environ(source: str) -> str:
    return replace_once(
        source,
        """    SGLANG_PCIE_IPC_TUNE = EnvBool(False)
""",
        """    SGLANG_PCIE_IPC_TUNE = EnvBool(False)
    # Serve all-reduces larger than SGLANG_PCIE_IPC_MAX_NUMEL as row chunks
    # through the same workspace instead of falling back to NCCL.
    SGLANG_PCIE_IPC_CHUNKED = EnvBool(False)
""",
        "PCIe IPC environment",
    )


def apply(path: Path, expected: str, patch) -> str:
    source = path.read_text()
    actual = hashlib.sha256(source.encode()).hexdigest()
    if actual != expected:
        raise SystemExit(f"{path}: expected SHA-256 {expected}, got {actual}")
    patched = patch(source)
    compile(patched, str(path), "exec")
    path.write_text(patched)
    return hashlib.sha256(patched.encode()).hexdigest()


def main() -> None:
    for path, expected, patch in (
        (PARALLEL_STATE, PARALLEL_STATE_SHA256, patch_parallel_state),
        (ENVIRON, ENVIRON_SHA256, patch_environ),
    ):
        print(f"{path}: {apply(path, expected, patch)}")


if __name__ == "__main__":
    main()
