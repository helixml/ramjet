#!/usr/bin/env python3
"""v0.6.1 port: serve TP2 prefill all-reduces as chunks through the PCIe IPC workspace.

Same change as deploy/glm53_flash_sm120/patch-pcie-ipc-chunked.py, against
upstream SGLang's ``PcieIpcCommunicator`` (ormandj v0.6.1 image). Upstream keeps
prefill-sized reductions on NCCL because a prefill-sized *single-call*
workspace measured +66% TTFT at TP8 across two sockets. At TP2 on one socket
NCCL falls back to host-memory SHM, and 192-row chunks through the
decode-sized workspace measured 1.46 ms vs 2.30 ms for a 6,144 x 4,096 BF16
all-reduce. The chunk path is therefore admitted only for two-rank groups and
only with ``SGLANG_PCIE_IPC_CHUNKED=1``.
"""

from __future__ import annotations

import hashlib
from pathlib import Path


SRC = Path("/opt/glm53/sources/sglang/python/sglang/srt")
COMM = SRC / "distributed/device_communicators/pcie_ipc_ar.py"
COMM_SHA256 = "ce9671ab87ace6d3fcddbcffb4cea4b698560abb35f22d8bff74afa53b8605bc"
ENVIRON = SRC / "environ.py"
ENVIRON_SHA256 = "7cc569f712385d3e98dba3f70676c87f5c17910aee48cddb377c00e73afe0c69"


def replace_once(source: str, old: str, new: str, what: str) -> str:
    if source.count(old) != 1:
        raise SystemExit(f"pinned source shape changed: {what}")
    return source.replace(old, new, 1)


def patch_comm(source: str) -> str:
    source = replace_once(
        source,
        """        if not self._ensure_workspace(inp):
            return False
        if inp.numel() > self.max_numel:
            return False
        return self._workspace.supports(inp)
""",
        """        if not self._ensure_workspace(inp):
            return False
        if inp.numel() > self.max_numel:
            rows = self._chunk_rows(inp)
            return rows > 0 and self._workspace.supports(inp[:rows])
        return self._workspace.supports(inp)

    def _chunk_rows(self, inp: torch.Tensor) -> int:
        \"\"\"Rows per chunk for reductions wider than the workspace, or 0 for NCCL.

        Only two-rank groups: there NCCL stages through host memory (SHM) when
        the pair meets at a PCIe host bridge, and workspace-sized chunks beat
        it at every size measured (6144x4096 bf16: 1.46 ms vs 2.30 ms).
        \"\"\"
        if self._world_size != 2 or not envs.SGLANG_PCIE_IPC_CHUNKED.get():
            return 0
        return self.max_numel // inp.shape[-1]
""",
        "PCIe IPC admission",
    )
    return replace_once(
        source,
        """            self._workspace.rebind_stream()
            self._bound_stream = stream
        return self._workspace.all_reduce(inp)
""",
        """            self._workspace.rebind_stream()
            self._bound_stream = stream
        if inp.numel() <= self.max_numel:
            return self._workspace.all_reduce(inp)
        rows = self._chunk_rows(inp)
        flat = inp.view(-1, inp.shape[-1])
        out = torch.empty_like(flat)
        for r0 in range(0, flat.shape[0], rows):
            self._workspace.all_reduce(flat[r0 : r0 + rows], out=out[r0 : r0 + rows])
        return out.view(inp.shape)
""",
        "PCIe IPC all-reduce",
    )


def patch_environ(source: str) -> str:
    return replace_once(
        source,
        """    SGLANG_PCIE_IPC_MAX_NUMEL = EnvInt(0)
""",
        """    SGLANG_PCIE_IPC_MAX_NUMEL = EnvInt(0)
    # TP2 only: serve all-reduces wider than the PCIe IPC workspace as row
    # chunks through it instead of NCCL.
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
        (COMM, COMM_SHA256, patch_comm),
        (ENVIRON, ENVIRON_SHA256, patch_environ),
    ):
        print(f"{path}: {apply(path, expected, patch)}")


if __name__ == "__main__":
    main()
