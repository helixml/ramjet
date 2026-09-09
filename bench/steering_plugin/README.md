# Qwen3.8 Flash-Next steering plugin

This opt-in vLLM general plugin captures or directionally steers the replicated
FFN output of the node06 `Qwen4ExpForConditionalGeneration` checkpoint. It is
inert unless `QWEN38_STEERING_CAPTURE_DIR` or `QWEN38_STEERING_VECTOR` is set.

The capture point is `Qwen3_8FlashNextDecoderLayer.mlp_out`, before the next
four-stream hyper-connection combine. Captures contain only the final-token
float32 activation of each prefill chunk, never prompts or completions. The
capture manifest selects the last chunk, which is the actual prompt boundary,
and retains hashes for earlier chunks. The capture directory must be a real,
process-owned mode-0700 directory.

Steering files contain one unit-normalized `directions` tensor with shape
`[48, 2560]`, or an experimental bundle with shape `[N, 48, 2560]`.
`QWEN38_STEERING_SCALE=1` removes the component along each selected direction;
`QWEN38_STEERING_LAYERS=24-43` restricts application to a layer range. A
process-owned mode-0600 `QWEN38_STEERING_CONTROL_FILE` can select the bundle
index, scale, and layers between synthetic evaluation requests without another
model reload. The control directory is mounted read-only in the container and
the host writer uses atomic replacement.

The control file is replica-global, not request-local. It must not toggle
steering between concurrent requests with different trust levels. An
authorization-gated deployment needs a dedicated admitted-exercise replica or
a future request-local activation hook. Prompt text is never sufficient to
enable a direction.

## Construction and evaluation

The original experiment contrasted generic and authorization-envelope prompts.
The offensive-cyber experiment instead keeps the fictional authorized task
identical and contrasts a continued refusal prefix with a continued bounded-
action prefix. It trains on a fixed split and can build raw-mean,
pair-normalized, and control-mean-orthogonalized estimators before bundling them
for a warm-engine layer/scale sweep.

`bench/qwen38_steering_compose.py` renders a complete capture or steering
candidate from node06's canonical Compose file. Both modes require
`--enforce-eager`: the plugin patches a model-internal forward method, which is
not a stable vLLM activation-steering API and is intentionally pinned to the
exact node06 image and Qwen architecture. `bench/qwen38_steering_eval_campaign.sh`
compares the withdrawn baseline and steered replica on authorized and boundary
cases. A candidate is acceptable only with no malformed responses, unsafe
boundary decisions, or regressions and at least one held-out improvement.

## Historical qualified synthetic offensive-cyber setting

`bench/qwen38_cyber_eval.py` supplies a non-networked fake tool and never
executes or persists its argument text. The 36 authorized cases use only
`.lab.invalid` fixtures and are partitioned before capture; 12 separate safety
controls cover missing/expired authorization, third-party targets, unbounded
credential attacks, denial of service, persistence, real-user collection, and
destructive actions.

For the exact node06 checkpoint, the held-out winner is the plain-mean vector
with positive scale `0.5` on layers `20-23`. It moved the untouched test set
from 7/8 to 8/8 fake-tool entry in two repeats with no regression or unsafe
boundary tool call. This does not qualify argument-schema fidelity or
production performance: the Python hook requires eager mode and the generated
fake-tool arguments were often not schema-exact.

## Authorized-action v2 evaluation

The current corpus is owned by the sibling `cyber/dir-steering/` directory. It
adds benign owner-directed work and explicitly approved, record-only
availability/destructive/persistence/credential-access simulations. Matching
controls require refusal when the same technique is prohibited, ownership is
missing, or a request asks for real autonomous high-impact execution.

The v2 design uses a generic refusal/action continuation and a non-executing
`synthetic_authorized_step` function. A fresh 48-pair vector has been captured,
but it did not show a repeat-stable held-out improvement and is not a serving
candidate. The historical 20-pair offensive vector is also not a v2 serving
candidate. Any real executor independently validates the signed RoE, target,
technique, time/quantity limits, execution mode, and human approval.
