"""Opt-in Qwen3.8 Flash-Next activation capture and directional steering.

The plugin is inert unless capture or steering environment variables are set.
It patches the model's replicated FFN output, immediately before the next
hyper-connection combine. Only tensor-parallel rank zero persists captures.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import stat
import threading
from typing import Any


CAPTURE_DIR_ENV = "QWEN38_STEERING_CAPTURE_DIR"
CAPTURE_ENABLE_ENV = "QWEN38_STEERING_CAPTURE_ENABLE_FILE"
VECTOR_ENV = "QWEN38_STEERING_VECTOR"
SCALE_ENV = "QWEN38_STEERING_SCALE"
LAYERS_ENV = "QWEN38_STEERING_LAYERS"
CONTROL_FILE_ENV = "QWEN38_STEERING_CONTROL_FILE"

_PATCHED = False
_LOCK = threading.Lock()
_CAPTURE: dict[str, Any] | None = None
_CAPTURE_SEQUENCE = 0


def parse_layers(value: str | None, layer_count: int) -> frozenset[int]:
    """Parse ``1,4-7`` into validated zero-based layer indexes."""
    if layer_count <= 0:
        raise ValueError("layer_count must be positive")
    if value is None or not value.strip() or value.strip().lower() == "all":
        return frozenset(range(layer_count))
    selected: set[int] = set()
    for item in value.split(","):
        item = item.strip()
        if not item:
            raise ValueError("empty layer selector")
        match = re.fullmatch(r"([0-9]+)(?:-([0-9]+))?", item)
        if match is None:
            raise ValueError(f"invalid layer selector: {item!r}")
        start = int(match.group(1))
        end = int(match.group(2) or start)
        if start > end:
            raise ValueError(f"descending layer range: {item!r}")
        if end >= layer_count:
            raise ValueError(f"layer selector outside 0-{layer_count - 1}: {item!r}")
        selected.update(range(start, end + 1))
    return frozenset(selected)


def _private_directory(path: str) -> pathlib.Path:
    directory = pathlib.Path(path)
    if directory.is_symlink() or not directory.is_dir():
        raise RuntimeError(f"capture directory is not a real directory: {directory}")
    info = directory.stat()
    if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise RuntimeError("capture directory must be owned by this process and mode 0700")
    return directory


def _existing_regular_file(path: str) -> pathlib.Path:
    candidate = pathlib.Path(path)
    if candidate.is_symlink() or not candidate.is_file():
        raise RuntimeError(f"steering vector is not a real file: {candidate}")
    return candidate


def _read_control(
    path: pathlib.Path, layer_count: int, direction_count: int
) -> tuple[float, frozenset[int], int, tuple[int, int, int, int]]:
    info = path.lstat()
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise RuntimeError(f"steering control is not a regular file: {path}")
    if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o600:
        raise RuntimeError("steering control must be process-owned and mode 0600")
    if not 1 <= info.st_size <= 4096:
        raise RuntimeError("steering control size is outside 1..4096 bytes")
    document = json.loads(path.read_text())
    if not isinstance(document, dict) or set(document) - {
        "direction_index",
        "generation",
        "layers",
        "scale",
    }:
        raise RuntimeError("steering control has unknown fields or is not an object")
    try:
        scale = float(document["scale"])
        direction_index = int(document["direction_index"])
        layers = parse_layers(document["layers"], layer_count)
    except (AttributeError, KeyError, TypeError, ValueError) as error:
        raise RuntimeError("steering control has invalid values") from error
    if not (-8.0 <= scale <= 8.0):
        raise RuntimeError("steering control scale must be within [-8, 8]")
    if not 0 <= direction_index < direction_count:
        raise RuntimeError("steering control direction index is outside the bundle")
    identity = (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_size)
    return scale, layers, direction_index, identity


def _next_capture_path(directory: pathlib.Path) -> pathlib.Path:
    global _CAPTURE_SEQUENCE
    with _LOCK:
        while True:
            candidate = directory / f"capture-{_CAPTURE_SEQUENCE:06d}.safetensors"
            _CAPTURE_SEQUENCE += 1
            if not candidate.exists():
                return candidate


def register() -> None:
    """vLLM general-plugin entry point."""
    global _PATCHED
    if _PATCHED:
        return

    capture_dir_value = os.environ.get(CAPTURE_DIR_ENV)
    vector_value = os.environ.get(VECTOR_ENV)
    control_value = os.environ.get(CONTROL_FILE_ENV)
    if not capture_dir_value and not vector_value and not control_value:
        return
    if control_value and not vector_value:
        raise RuntimeError(f"{CONTROL_FILE_ENV} requires {VECTOR_ENV}")

    # Imports stay lazy so CPU-only repository tests need no torch/vLLM install.
    import torch
    from safetensors.torch import load_file, save_file
    from vllm.distributed import get_tensor_model_parallel_rank
    try:
        from vllm.models.qwen4_exp.nvidia.model import (
            Qwen4ExpDecoderLayer as Qwen3_8FlashNextDecoderLayer,
        )
    except ModuleNotFoundError:
        from vllm.models.qwen3_8_flash_next.nvidia.model import (
            Qwen3_8FlashNextDecoderLayer,
        )

    layer_count = 48
    hidden_size = 2560
    capture_dir = _private_directory(capture_dir_value) if capture_dir_value else None
    enable_file = pathlib.Path(
        os.environ.get(CAPTURE_ENABLE_ENV, str(capture_dir / "enabled") if capture_dir else "")
    )
    layers = parse_layers(os.environ.get(LAYERS_ENV), layer_count)
    try:
        scale = float(os.environ.get(SCALE_ENV, "0"))
    except ValueError as error:
        raise RuntimeError(f"invalid {SCALE_ENV}") from error
    if not (-8.0 <= scale <= 8.0):
        raise RuntimeError(f"{SCALE_ENV} must be within [-8, 8]")

    directions = None
    direction_count = 0
    if vector_value:
        vector_path = _existing_regular_file(vector_value)
        loaded = load_file(str(vector_path), device="cpu")
        if set(loaded) != {"directions"}:
            raise RuntimeError("steering file must contain only a directions tensor")
        directions = loaded["directions"].float().contiguous()
        if tuple(directions.shape) == (layer_count, hidden_size):
            directions = directions.unsqueeze(0)
        if directions.ndim != 3 or tuple(directions.shape[1:]) != (
            layer_count,
            hidden_size,
        ):
            raise RuntimeError(
                "directions shape must be [48, 2560] or [N, 48, 2560], "
                f"got {tuple(directions.shape)}"
            )
        norms = torch.linalg.vector_norm(directions, dim=-1)
        if not torch.isfinite(directions).all() or not torch.allclose(
            norms, torch.ones_like(norms), atol=2e-4, rtol=2e-4
        ):
            raise RuntimeError("every steering direction must be finite and unit-normalized")
        direction_count = directions.shape[0]

    control_file = pathlib.Path(control_value) if control_value else None
    control_identity = None
    direction_index = 0
    if control_file is not None:
        scale, layers, direction_index, control_identity = _read_control(
            control_file, layer_count, direction_count
        )

    original_forward = Qwen3_8FlashNextDecoderLayer.forward
    device_cache: dict[tuple[int, int, str, torch.dtype], torch.Tensor] = {}

    def refresh_control() -> None:
        nonlocal scale, layers, direction_index, control_identity
        if control_file is None:
            return
        info = control_file.lstat()
        identity = (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_size)
        if identity == control_identity:
            return
        with _LOCK:
            updated = _read_control(control_file, layer_count, direction_count)
            scale, layers, direction_index, control_identity = updated

    def wrapped_forward(self: Any, *args: Any, **kwargs: Any) -> Any:
        global _CAPTURE
        result = original_forward(self, *args, **kwargs)
        hidden_states, mlp_out, injection = result
        layer_id = int(self.layer_idx)
        if layer_id == 0:
            refresh_control()

        query_start_loc = kwargs.get("query_start_loc")
        sequence_count = (
            query_start_loc.numel() - 1 if query_start_loc is not None else 0
        )
        capture_active = (
            capture_dir is not None
            and enable_file.is_file()
            and query_start_loc is not None
            and sequence_count >= 1
            # vLLM may retain full sequence boundaries for a one-token decode.
            # The actual FFN row count is the authoritative prefill/decode split.
            and mlp_out.shape[0] > sequence_count
            and get_tensor_model_parallel_rank() == 0
        )
        if capture_active:
            # Fixed-slot attention metadata pads zero-span sequences whose
            # boundary duplicates the real sequence end, duplicating its row.
            nonzero = (query_start_loc[1:] > query_start_loc[:-1])
            ends = (query_start_loc[1:] - 1).to(device=mlp_out.device, dtype=torch.long)
            ends = ends[nonzero.to(device=ends.device)]
            selected = mlp_out.index_select(0, ends).detach().float().cpu()
            if layer_id == 0:
                _CAPTURE = {
                    "layers": [None] * layer_count,
                    "sequence_count": selected.shape[0],
                    "processed_tokens": mlp_out.shape[0],
                    "max_query_span": int(
                        (query_start_loc[1:] - query_start_loc[:-1]).max().item()
                    ),
                }
            state = _CAPTURE
            if state is None or state["sequence_count"] != selected.shape[0]:
                raise RuntimeError("steering capture lost decoder-layer alignment")
            state["layers"][layer_id] = selected
            if layer_id == layer_count - 1:
                if any(value is None for value in state["layers"]):
                    raise RuntimeError("steering capture is missing a decoder layer")
                activations = torch.stack(state["layers"], dim=1).contiguous()
                output = _next_capture_path(capture_dir)
                temporary = output.with_suffix(".tmp")
                save_file(
                    {"activations": activations},
                    str(temporary),
                    metadata={
                        "architecture": "Qwen4ExpForConditionalGeneration",
                        "capture_point": "decoder.mlp_out.before_hyperconnection_combine",
                        "processed_tokens": str(state["processed_tokens"]),
                        "max_query_span": str(state["max_query_span"]),
                    },
                )
                os.chmod(temporary, 0o600)
                os.replace(temporary, output)
                _CAPTURE = None

        if directions is not None and scale != 0.0 and layer_id in layers:
            key = (direction_index, layer_id, str(mlp_out.device), mlp_out.dtype)
            direction = device_cache.get(key)
            if direction is None:
                direction = directions[direction_index, layer_id].to(
                    device=mlp_out.device, dtype=mlp_out.dtype
                )
                device_cache[key] = direction
            coefficient = torch.matmul(mlp_out.float(), direction.float())
            mlp_out = mlp_out - (
                scale * coefficient.unsqueeze(-1).to(mlp_out.dtype) * direction
            )

        return hidden_states, mlp_out, injection

    Qwen3_8FlashNextDecoderLayer.forward = wrapped_forward
    _PATCHED = True


__all__ = ["parse_layers", "register"]
