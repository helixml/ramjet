"""Serve DeepSeek-V4.1's 32x32 block-FP8 dense linears as BF16 on Hopper.
The 32-wide ue8m0 blocks keep these layers on SGLang's Triton block-FP8 kernel,
which runs far below bandwidth roofline at decode batch sizes. FP8 x power-of-two
scale is exact in BF16, so dequantizing once at load and calling cuBLAS is lossless
for the weights and also skips activation quantization. Opt-in:
SGLANG_BLOCK_FP8_DEQUANT_BF16=1. Layers whose weight the model reads directly
(keep_plain_weight_layout, i.e. DSv4 wo_a) are left alone."""
import pathlib, sys
root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/sgl-workspace/sglang/python/sglang")

def edit(rel, old, new):
    p = root / rel; s = p.read_text()
    if new in s:  # a container restart re-runs the start command
        print("already patched", rel); return
    if s.count(old) != 1:
        raise SystemExit(f"anchor not unique/found in {rel}: {old[:80]!r} ({s.count(old)})")
    p.write_text(s.replace(old, new)); print("patched", rel)

edit("srt/layers/quantization/fp8.py",
'''        layer.weight.data = weight.data
        layer.weight_scale_inv.data = weight_scale.data
        if self.block_fp8_as_mxfp8:
            self._prepare_block_fp8_as_mxfp8(layer)
''',
'''        layer.weight.data = weight.data
        layer.weight_scale_inv.data = weight_scale.data
        if self.block_fp8_as_mxfp8:
            self._prepare_block_fp8_as_mxfp8(layer)
        import os as _os
        if (
            _os.environ.get("SGLANG_BLOCK_FP8_DEQUANT_BF16", "0") == "1"
            and self.block_quant
            and not self.use_mxfp8
            and not getattr(layer, "keep_plain_weight_layout", False)
            and layer.weight.dim() == 2
            and layer.weight.dtype == torch.float8_e4m3fn
        ):
            bn, bk = self.weight_block_size
            n, k = layer.weight.shape
            s = layer.weight_scale_inv.data.float()
            s = s.repeat_interleave(bn, dim=0)[:n].repeat_interleave(bk, dim=1)[:, :k]
            layer.weight_bf16 = (layer.weight.data.float() * s).to(torch.bfloat16).contiguous()
            del s
''')

edit("srt/layers/quantization/fp8.py",
'''        mxfp8_view = self.use_mxfp8 or (
            self.block_fp8_as_mxfp8 and layer.block_fp8_mxfp8_ready
        )
''',
'''        w_bf16 = getattr(layer, "weight_bf16", None)
        if w_bf16 is not None and isinstance(x, torch.Tensor) and x.dtype == torch.bfloat16:
            return torch.nn.functional.linear(x, w_bf16, bias)
        mxfp8_view = self.use_mxfp8 or (
            self.block_fp8_as_mxfp8 and layer.block_fp8_mxfp8_ready
        )
''')
