#!/bin/bash
# Apply the experiment's SGLang patches to the image's source tree, then serve.
# sglang-patches/*.py are new modules; *.patch are unified diffs against
# /opt/sglang-source/python of image sha256:899fe8eb… (applied in name order).
set -euo pipefail
src=/opt/sglang-source/python
patches=/opt/kexp/sglang-patches
cp "$patches/w8a16_tilelang.py" "$src/sglang/srt/layers/quantization/w8a16_tilelang.py"
for p in "$patches"/*.patch; do
  patch -d "$src" -p1 --forward --batch < "$p"
done
exec sglang "$@"
