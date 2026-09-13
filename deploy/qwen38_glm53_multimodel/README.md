# Qwen3.8-Flash-Next + GLM-5.3-Flash behind one Ramjet

This is the node06 Ramjet-only deployment for the two independently managed
engines that are currently resident together:

- `qwen38flashnext-a`: Qwen3.8-Flash-Next, TP4 on GPUs 0-3.
- `glm53sm120-b`: GLM-5.3-Flash, TP2 on GPUs 4-5.

The engine Compose projects retain their own lifecycle and immutable model and
runtime pins. This deployment owns only `ds4-loadbalancer`, joins both external
engine networks, and exposes their exact served IDs through one combined
`GET /v1/models`. Requests selecting `qwen3.8-flash-next` can reach only the
Qwen engine; requests selecting `glm-5.3-flash` can reach only the GLM engine.
Unknown, absent, or malformed model selection is rejected before any upstream
is dialed.

Machine view also publishes this static ownership map on its Topology tab:
Qwen is Engine A (`TP4`, GPUs 0-3) and GLM is Engine B (`TP2`, GPUs 4-5).
That serving view is independent of the optional adaptive controller and stays
visible while adaptive topology changes are disabled.

## Deliberate feature boundary

The deployment disables local tokenization, exact routing, direct/snapshot KV
events, adaptive topology, speculative-profile placement, prefix single-flight,
affinity-horizon estimation, and idle parking. Those features currently own one
fleet-wide tokenizer, compatibility manifest, event geometry, or interchangeable
replica set. Applying the Qwen authority to GLM would make their telemetry or
placement incorrect. Ordinary prefix/load accounting remains active but has one
eligible TP engine per model, so it cannot cross the ownership boundary.

The LB has no Docker socket and cannot start, stop, or recreate either engine.
Direct engine ports remain loopback-only. Public access continues through the
existing authenticated reverse proxy on `127.0.0.1:8006`.

## Validate

The Compose file requires an immutable `LB_IMAGE` and the existing protected
credential file. The validator supplies non-secret sentinels and checks the
rendered topology:

```bash
python3 deploy/qwen38_glm53_multimodel/validate-compose.py
bash -n deploy/qwen38_glm53_multimodel/node06-rollout.sh
```

Before rollout, verify both source projects and external networks are present,
and record the running LB identity and complete Compose file list. Do not start
or recreate either engine from this directory.

## Roll out on node06

Use the exact `rust-<revision>@sha256:<digest>` image produced for the merged
commit. Copy this directory to a protected node06 path, then run the rollout
under the repository thermal guard. Model load is not involved; the bound is
for four small inference probes plus two LB starts:

```bash
cd /path/to/qwen38_glm53_multimodel
sudo python3 /home/luke/inference/glm53_flash_sm120/node06_gpu_guard.py \
  --label qwen38-glm53-multimodel-lb \
  --output /protected/evidence/thermal.jsonl \
  --max-runtime-seconds 300 \
  -- env LB_IMAGE='ghcr.io/helixml/ramjet:rust-REV@sha256:DIGEST' \
    ./node06-rollout.sh
```

The script holds `/run/lock/ramjet-node06-deployment.lock`, verifies both
engine containers and all three external networks, and first starts the exact
candidate on alternate loopback ports. It requires a combined two-model list
and one successful request to each owner before the public LB is touched.
Set `RAMJET_CANARY_ONLY=1` to stop after that proof and remove the alternate-port
container without touching the public LB; this is the pre-merge qualification
mode.

For promotion it stops and renames the old LB instead of deleting it, starts
the new canonical container under a release-unique Compose project, and repeats
the same checks. The unique project is required because Compose v5 otherwise
rediscovers the renamed old service by label and recreates it, consuming the
rollback artifact. Any failure removes
the candidate/new LB, restores the old container's exact configuration and
image under its original name, and starts it. On success the script prints the
name of the stopped rollback container; retain that container until the new LB
has passed the observation window.

The promoted container deliberately carries a release-unique Compose project
label. For later inspection or an exact Compose operation, read that authority
from the running container instead of assuming the file's default project:

```bash
project=$(docker inspect ds4-loadbalancer \
  --format '{{index .Config.Labels "com.docker.compose.project"}}')
docker compose --env-file /path/to/protected.env -p "$project" \
  -f docker-compose.yaml ps
```

## Verify and roll back

Verify health, the combined IDs, both owner-routed requests, and the two fixed
upstream metric series. Do not print prompts, completions, credentials, or raw
request bodies into the deployment journal.

To roll back after a successful script run, stop and remove only the new
`ds4-loadbalancer`, rename the printed stopped rollback container back to
`ds4-loadbalancer`, and start it. This restores the byte-identical old container
without rendering its historical Compose inputs. Qwen A and GLM B stay running
through either LB operation.
