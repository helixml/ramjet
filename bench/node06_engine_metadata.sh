#!/usr/bin/env bash
# Capture one engine's privacy-safe immutable identity and optionally verify a receipt.
# Usage: node06_engine_metadata.sh OUTPUT_JSON ENGINE_CONTAINER [RECEIPT_JSON]
set -euo pipefail

output=${1:?usage: node06_engine_metadata.sh OUTPUT_JSON ENGINE_CONTAINER [RECEIPT_JSON]}
container=${2:?usage: node06_engine_metadata.sh OUTPUT_JSON ENGINE_CONTAINER [RECEIPT_JSON]}
receipt=${3:-}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
model_root=${BENCH_MODEL_ROOT:-/prod/models/sglang/DeepSeek-V4-Flash-0731}

for command in docker jq sha256sum nvidia-smi python3 timeout; do
  command -v "$command" >/dev/null || { echo "missing command: $command" >&2; exit 1; }
done
for path in "$model_root/tokenizer.json" "$model_root/config.json"; do
  [[ -r $path ]] || { echo "missing model artifact: $path" >&2; exit 1; }
done
[[ -z $receipt || -r $receipt ]] || { echo "unreadable receipt: $receipt" >&2; exit 1; }

temporary=$(mktemp)
trap 'rm -f "$temporary"' EXIT

configured_image=$(docker inspect --format '{{.Config.Image}}' "$container")
image_id=$(docker inspect --format '{{.Image}}' "$container")
image_descriptor_digest=$(
  docker image inspect "$image_id" --format '{{if .Descriptor}}{{.Descriptor.Digest}}{{end}}'
)
repo_digests=$(docker image inspect "$image_id" --format '{{json .RepoDigests}}')
image_config_digest=""
if command -v docker >/dev/null && docker buildx version >/dev/null 2>&1; then
  manifest_json=$(
    timeout 30s docker buildx imagetools inspect --raw "$configured_image" \
      2>/dev/null || true
  )
  if [[ -n $manifest_json ]]; then
    image_config_digest=$(jq -r '.config.digest // empty' <<<"$manifest_json")
  fi
fi
command_line=$(docker exec "$container" pgrep -af 'vllm serve' | head -1)
[[ -n $command_line ]] || { echo "vllm serve process not found: $container" >&2; exit 1; }

read_flag() {
  local wanted=$1
  python3 - "$wanted" "$command_line" <<'PY'
import shlex, sys
wanted, command = sys.argv[1:]
words = shlex.split(command)
for index, word in enumerate(words):
    if word == wanted and index + 1 < len(words):
        print(words[index + 1])
        break
    if word.startswith(wanted + "="):
        print(word.split("=", 1)[1])
        break
PY
}

model_revision=$(read_flag --revision)
tokenizer_revision=$(read_flag --tokenizer-revision)
tokenizer_revision=${tokenizer_revision:-$model_revision}
[[ -n $model_revision && -n $tokenizer_revision ]] || {
  echo "engine argv lacks immutable model/tokenizer revision: $container" >&2
  exit 1
}

runtime_packages=$(
  docker exec -i "$container" python3 - <<'PY'
import importlib.metadata as metadata
import json

aliases = {
    "vllm": ("vllm",),
    "torch": ("torch",),
    "b12x": ("b12x",),
    "flashinfer": ("flashinfer-python", "flashinfer_python"),
    "lmcache": ("lmcache",),
}
result = {}
for output, names in aliases.items():
    for name in names:
        try:
            result[output] = metadata.version(name)
            break
        except metadata.PackageNotFoundError:
            pass
print(json.dumps(result, sort_keys=True))
PY
)

tokenizer_sha256=$(sha256sum "$model_root/tokenizer.json" | cut -d' ' -f1)
config_sha256=$(
  find "$model_root" -maxdepth 2 -type f \
    \( -name config.json -o -name generation_config.json -o \
       -name tokenizer_config.json -o -name encoding_dsv4.py \) \
    -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1
)
topology_sha256=$(nvidia-smi topo -m | sha256sum | cut -d' ' -f1)
driver=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -1)

jq -n \
  --arg captured_utc "$(date -u +%FT%TZ)" \
  --arg container "$container" \
  --arg configured_image "$configured_image" \
  --arg image_id "$image_id" \
  --arg image_descriptor_digest "$image_descriptor_digest" \
  --arg image_config_digest "$image_config_digest" \
  --argjson repo_digests "${repo_digests:-[]}" \
  --arg model_revision "$model_revision" \
  --arg tokenizer_revision "$tokenizer_revision" \
  --arg tokenizer_sha256 "$tokenizer_sha256" \
  --arg config_sha256 "$config_sha256" \
  --arg driver "$driver" \
  --arg topology_sha256 "$topology_sha256" \
  --arg started_at "$(docker inspect --format '{{.State.StartedAt}}' "$container")" \
  --argjson restart_count "$(docker inspect --format '{{.RestartCount}}' "$container")" \
  --arg cpuset_cpus "$(docker inspect --format '{{.HostConfig.CpusetCpus}}' "$container")" \
  --arg cpuset_mems "$(docker inspect --format '{{.HostConfig.CpusetMems}}' "$container")" \
  --arg command "$command_line" \
  --argjson runtime_packages "$runtime_packages" \
  '{captured_utc:$captured_utc,container:$container,configured_image:$configured_image,
    image_id:$image_id,image_descriptor_digest:$image_descriptor_digest,
    image_config_digest:$image_config_digest,repo_digests:$repo_digests,
    model_revision:$model_revision,
    tokenizer_revision:$tokenizer_revision,tokenizer_sha256:$tokenizer_sha256,
    config_sha256:$config_sha256,driver:$driver,topology_sha256:$topology_sha256,
    started_at:$started_at,restart_count:$restart_count,cpuset_cpus:$cpuset_cpus,
    cpuset_mems:$cpuset_mems,runtime_packages:$runtime_packages,command:$command}' \
  >"$temporary"

arguments=("$temporary")
[[ -z $receipt ]] || arguments+=("$receipt")
python3 "$script_dir/engine_identity.py" "${arguments[@]}" --output "$output"
chmod 0600 "$output"
echo "engine identity written: $output" >&2
