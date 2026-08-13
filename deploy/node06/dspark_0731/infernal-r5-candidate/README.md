# Infernal r5 V4 correctness candidate

This is an immutable, source-locked thin overlay on the exact rejected
Infernal r4 image. It changes only Python source under the inherited
`/opt/infernal-invocation/vllm` checkout:

- the r4-adapted semantic port of vLLM #51318;
- the DeepSeek V4 runtime subset of vLLM #49117; and
- the exact inline/LF `toolcalls` prefix extension required by #51914.

It does not change B12X, FlashInfer, LMCache, InstantTensor, Torch, CUDA, NCCL,
native vLLM extensions, launcher arguments, or model/tokenizer revisions.
`manifest.json` pins every input and resulting source identity.

Prepare and qualify source without Docker or GPUs:

```bash
R4_SOURCE=/path/to/exact/r4-vllm ./build.sh
```

That proves the exact r4 tree and runs both committed preflights before and
after applying the patch. It also requires the patch's exact six-file allowlist,
compiles those Python sources without importing them or writing bytecode, and
proves the ordinary wrapped-parallel r4 parser result is unchanged. The
source-only gate took 1.15s on the 2026-08-13 development checkout.

Local image construction is an explicit second step (`BUILD_IMAGE=1`); the
script intentionally cannot push. Do not use the Compose overlay until the
built image has a content digest and the ordinary candidate
smoke/corruption/scout gates authorize node06 work.
