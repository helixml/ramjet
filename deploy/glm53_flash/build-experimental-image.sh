#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "build-experimental-image.sh: $*" >&2
  exit 2
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
source_dir=${SOURCE_DIR:-}
image_tag=${IMAGE_TAG:-ramjet/glm53-flash-nvfp4-sglang-sm120:sero-8370bb0}
expected_commit=8370bb04335bb07b6ee85907dd83cd1d300fa462

[[ -n $source_dir && $source_dir == /* ]] || \
  fail "set SOURCE_DIR to an absolute clone of the reviewed third-party source"
[[ -d $source_dir/.git ]] || fail "SOURCE_DIR is not a Git checkout"
[[ $(git -C "$source_dir" rev-parse HEAD) == "$expected_commit" ]] || \
  fail "third-party source is not at the reviewed commit"

build_dir=$(mktemp -d /tmp/ramjet-glm53-image.XXXXXX)
trap 'rm -rf -- "$build_dir"' EXIT
git -C "$source_dir" archive "$expected_commit" | tar -x -C "$build_dir"
patch --quiet -d "$build_dir" -p1 < "$script_dir/third-party-dockerfile.patch"

(
  cd "$build_dir"
  sha256sum -c <<'EOF'
aeb9eb145958644de548b15031842c8b8c3daac6daa73139d5c676a7a2b211da  patches/sglang-glm5_next-debug.py
dce70ed7392702154e69433f8faf498fb322eca4890683eb54d7857ae69aaf5a  patches/sglang-deepseek_nextn-glm53.py
0814b359c0be04b7021ac0da96a3c58fea83d5c6290483df5b509fac9f196ecf  patches/sglang-quant-utils-sm120.py
10367ba1573c11e5b3c7e88195dab77f0cf76169e5db4dd9baf6d366a8652ead  patches/sglang-modelopt-quant-sm120.py
06c7f42a192997ab4be73dad926fe9553ea79708d0e5b1583e26720fc0ef6f8e  patches/sglang-flash_mla_sm120-glm53.py
1b786b161e9279b707f6bc24cb31b835bad1fc6a55f12d80dda47ee071636bc7  patches/sglang-dsa_backend-glm53.py
EOF
)

python3 - "$build_dir" <<'PY'
import ast
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
for path in sorted((root / "patches").glob("*.py")):
    ast.parse(path.read_text(), filename=str(path))
print("third-party Python parse passed")
PY

docker build --pull=false --tag "$image_tag" "$build_dir"
docker image inspect "$image_tag" --format '{{.Id}}'
