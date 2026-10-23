import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
PATCH_PATH = ROOT / "deploy/glm53_flash_sm120/patch-pcie-ipc-chunked.py"
SPEC = importlib.util.spec_from_file_location("patch_pcie_ipc_chunked", PATCH_PATH)
assert SPEC and SPEC.loader
PATCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PATCH)

PARALLEL_STATE = '''
class GroupCoordinator:
    def _can_use_pcie_ipc_allreduce(self, input_: torch.Tensor) -> bool:
        """Rank-invariant admission: shape, dtype, contiguity and capacity only."""
        ws = self.pcie_ipc_ws
        if ws is None or input_.dim() != 2:
            return False
        return ws.supports(input_)

    def _pcie_ipc_all_reduce(self, input_: torch.Tensor) -> torch.Tensor:
        ws = self.pcie_ipc_ws
        if not torch.cuda.is_current_stream_capturing():
            current = torch.cuda.current_stream(self.device)
            if current != self._pcie_ipc_stream:
                ws.rebind_stream()
                self._pcie_ipc_stream = current
        return ws.all_reduce(input_)
'''

ENVIRON = '''
class Envs:
    SGLANG_PCIE_IPC_MAX_NUMEL = EnvInt(786432)
    SGLANG_PCIE_IPC_TUNE = EnvBool(False)
'''


class FakeTensor:
    def __init__(self, rows, cols=4096, contiguous=True):
        self.shape = (rows, cols)
        self.contiguous = contiguous

    def dim(self):
        return 2

    def is_contiguous(self):
        return self.contiguous

    def __getitem__(self, s):
        start = s.start or 0
        stop = min(s.stop, self.shape[0])
        return FakeTensor(stop - start, self.shape[1], self.contiguous)


class FakeWorkspace:
    max_numel = 786432

    def __init__(self):
        self.calls = []

    def supports(self, t):
        return t.shape[0] * t.shape[1] <= self.max_numel

    def all_reduce(self, t, out=None):
        self.calls.append(t.shape[0])
        return out


class Flag:
    def __init__(self, value):
        self.value = value

    def get(self):
        return self.value


class Glm53PcieIpcChunkedPatchTests(unittest.TestCase):
    def coordinator(self, chunked):
        patched = PATCH.patch_parallel_state(PARALLEL_STATE)
        envs = type("envs", (), {})()
        envs.SGLANG_PCIE_IPC_CHUNKED = Flag(chunked)
        envs.SGLANG_PCIE_IPC_MAX_NUMEL = Flag(786432)
        torch = type("torch", (), {})()
        torch.Tensor = object
        torch.empty_like = lambda t: t
        torch.cuda = type("cuda", (), {"is_current_stream_capturing": staticmethod(lambda: True)})()
        scope = {"torch": torch, "envs": envs}
        exec(compile(patched, "parallel_state", "exec"), scope)
        coordinator = scope["GroupCoordinator"]()
        coordinator.pcie_ipc_ws = FakeWorkspace()
        return coordinator

    def test_prefill_tensors_are_served_in_workspace_sized_row_chunks(self):
        c = self.coordinator(chunked=True)
        x = FakeTensor(6144)
        self.assertTrue(c._can_use_pcie_ipc_allreduce(x))
        c._pcie_ipc_all_reduce(x)
        self.assertEqual(c.pcie_ipc_ws.calls, [192] * 32)

    def test_ragged_tail_chunk_is_served(self):
        c = self.coordinator(chunked=True)
        c._pcie_ipc_all_reduce(FakeTensor(200))
        self.assertEqual(c.pcie_ipc_ws.calls, [192, 8])

    def test_small_tensors_keep_the_single_call_path(self):
        c = self.coordinator(chunked=True)
        c._pcie_ipc_all_reduce(FakeTensor(24))
        self.assertEqual(c.pcie_ipc_ws.calls, [24])

    def test_disabled_flag_keeps_large_tensors_on_nccl(self):
        c = self.coordinator(chunked=False)
        self.assertFalse(c._can_use_pcie_ipc_allreduce(FakeTensor(6144)))
        self.assertTrue(c._can_use_pcie_ipc_allreduce(FakeTensor(192)))

    def test_non_contiguous_large_tensors_are_not_chunked(self):
        c = self.coordinator(chunked=True)
        self.assertFalse(c._can_use_pcie_ipc_allreduce(FakeTensor(6144, contiguous=False)))

    def test_environ_declares_the_flag_off_by_default(self):
        patched = PATCH.patch_environ(ENVIRON)
        self.assertIn("SGLANG_PCIE_IPC_CHUNKED = EnvBool(False)", patched)

    def test_patch_fails_closed_when_the_pinned_shape_changes(self):
        with self.assertRaises(SystemExit):
            PATCH.patch_parallel_state("unexpected upstream source")
        with self.assertRaises(SystemExit):
            PATCH.patch_environ(PARALLEL_STATE)


if __name__ == "__main__":
    unittest.main()
