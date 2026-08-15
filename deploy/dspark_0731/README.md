# DeepSeek V4 Flash deployment

This directory is the canonical source for node06's complete Docker Compose
stack: the Rust mini-dynamo load balancer and both TP4 vLLM/DSpark engines.
The deploy checkout on node06 and the infra repository contain mirrors so
operators can still use the established `/home/luke/inference/dspark_0731`
working directory.

## Active node06 cooling/AC moratorium

The 2026-08-14 moratorium overrides every node06 mutation and workload command
below, even if the host returns. Do not send inference requests, start/restart
either vLLM engine, load a model, run JIT/warmup, or apply a candidate/LB
deployment. AC repair alone is insufficient; resume only when the user
authorizes a specific supervised startup/workload/rollback window after the
repair. Until then, use this directory only for GPU-free development-host image
and manifest inspection, exact receipt checks, tests, and `docker compose
config` rendering that cannot contact or mutate node06.

Edit `docker-compose.yaml` here first. Keep secrets in node06's uncommitted
`.env`; never add them to either repository.

## Fast validation and mirroring

```bash
docker compose -f deploy/dspark_0731/docker-compose.yaml config --quiet
deploy/dspark_0731/sync-compose.sh --check ../infra
deploy/dspark_0731/sync-compose.sh ../infra
```

`--check` is read-only and exits nonzero if the infra mirror differs. The
sync command updates only the mirrored Compose file; it does not touch the
operational README, benchmark helper, `.env`, containers, or engines.

After the mini-dynamo and infra changes are merged, update node06 explicitly:

```bash
scp deploy/dspark_0731/docker-compose.yaml \
  node06:/home/luke/inference/dspark_0731/docker-compose.yaml
ssh node06 'cd /home/luke/inference/dspark_0731 && docker compose config --quiet'
```

Do not run an unqualified `docker compose up -d` after an engine setting
change: it may recreate both engines. For an LB-only promotion, always name
the service:

```bash
ssh node06 'cd /home/luke/inference/dspark_0731 && \
  docker compose up -d ds4-loadbalancer'
```

## Optional persistent JIT/autotune cache

`docker-compose.persistent-jit-cache.yaml` is a default-off, r34-specific
overlay. It pins the immutable engine digest and mounts distinct writable host
directories at `/cache/jit` for A and B. Never share the directories: compiler
and autotune writers are not a cross-process coordination protocol.

The exact image contains 26 cache directories and one zero-byte FlashInfer log
placeholder, but no reusable cache payload. Therefore an empty host bind does
not hide a compiled artifact for this digest. Prove that again for every new
image; the probe is intentionally not in Drone because the image is 12.5GB:

```bash
python3 bench/jit_cache_image_probe.py
python3 deploy/dspark_0731/validate-persistent-jit-cache-compose.py
```

Prepare the fixed node06 paths explicitly; Compose refuses to create them:

```bash
scp deploy/dspark_0731/docker-compose.persistent-jit-cache.yaml \
  deploy/dspark_0731/validate-persistent-jit-cache-host.sh \
  node06:/home/luke/inference/dspark_0731/
ssh node06 'install -d -o root -g root -m 0700 \
  /prod/mini-dynamo/jit-cache/vllme2666d9a65-b12x7cecbb2c48-136ce64f2c43f0f8/engine-a \
  /prod/mini-dynamo/jit-cache/vllme2666d9a65-b12x7cecbb2c48-136ce64f2c43f0f8/engine-b \
  && cd /home/luke/inference/dspark_0731 \
  && ./validate-persistent-jit-cache-host.sh \
  && docker compose -f docker-compose.yaml \
       -f docker-compose.persistent-jit-cache.yaml config --quiet'
```

Roll only one named engine while production is single-homed on its peer. Record
first-start readiness, every JIT/autotune marker, cache bytes/inodes, and the
same measurements after a second restart. The second start must be clean and
faster before retaining the overlay. Roll back by recreating that named engine
from the base file alone; do not delete the host cache during rollback.

`docker-compose.k5-block-canary.yaml` is a reproducible negative experiment,
not a production recommendation. It changes only engine B from the r34
K5/probabilistic/standard default to K5/probabilistic/block while production is
single-homed on A. Always merge it with the base file explicitly; restore B by
starting that named service from the base file alone. The block candidate failed
the agent-protocol gate recorded in `EXPERIMENTS.md` and must not be joined to
the load balancer.

## Opt-in in-process serving identity

`docker-compose.compatibility-identity.yaml` is the opt-in stack overlay for the
manifest-gated admission experiment. It mounts a standard-library-only ASGI
middleware, the SHA-pinned compatibility manifest, and a separately pinned
serving-runtime manifest into each vLLM frontend. The compatibility manifest
remains schema v1 at SHA-256
`4ae2503554fa7089bc455e2ee89af0677c5cabec523d6b08d91a93d9ec9259aa`;
the default-off schema-v2 runtime manifest links to that digest and pins one
EngineCore, the exact event/replay KV configuration, complete normalized vLLM
argv, selected non-secret environment, package versions, and launcher/NCCL
artifact hashes.
The exact authenticated `GET /v1/mini-dynamo/identity` path is answered in that
same API process; every inference request still goes directly to vLLM, without
a sidecar or another network hop. The middleware derives a fresh boot/process
incarnation from `/proc`, emits only the model/engine/tokenizer/renderer subset,
and refuses startup on a missing bearer, unsafe schema, non-regular manifest,
or digest mismatch. It also compares the live vLLM distribution version,
served model name, context limit, and tokenizer artifact hash with the
manifest before it can answer the endpoint. On the first authenticated control
request, it also proves the exact `AsyncLLM`/`AsyncMPClient` process structure,
captures the EngineCore boot/PID/start-time incarnation, matches the live typed
KV-event config, and verifies that the stable direct child owns exactly one
wildcard listener for each configured event/replay port in the frontend's
network namespace. It then makes bounded in-memory ASGI calls to the
initialized vLLM app for `/v1/models`, every committed `/tokenize` golden, and
`/health` before and after rendering. No loopback socket is opened. A complete
match is cached for that frontend process; every later identity request still
rechecks live health.
The whole first proof has a 4s deadline, below the LB's 5s admission deadline.
Because the probes deliberately traverse vLLM's real inner middleware and
routes, the first proof contributes one `/v1/models` and ten `/tokenize`
requests to vLLM's frontend HTTP metrics. Treat that bounded startup evidence
as control traffic when interpreting request counters.

Render and validate the candidate without starting an engine:

```bash
python3 deploy/dspark_0731/validate-serving-identity-compose.py
docker compose -f deploy/dspark_0731/docker-compose.yaml \
  -f deploy/dspark_0731/docker-compose.compatibility-identity.yaml \
  config --quiet
```

When the exact image is already cached, verify the real import path without a
GPU or network (0.47s warm on the 2026-08-14 development host):

```bash
docker run --rm --network none \
  --entrypoint /opt/venv/bin/python \
  --mount type=bind,src="$PWD/deploy/dspark_0731/engine_identity_middleware.py",dst=/opt/venv/lib/python3.12/site-packages/mini_dynamo_engine_identity.py,readonly \
  voipmonitor/vllm@sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b \
  -c 'from mini_dynamo_engine_identity import ServingIdentityMiddleware; print("identity middleware import passed")'
```

The stronger sub-second preflight runs the real launcher from rendered Compose
and compares all process evidence without starting vLLM or allocating a GPU:

    python3 bench/serving_runtime_image_probe.py
    python3 bench/serving_runtime_image_probe.py --service dspark-0731-b

The same capture path deterministically regenerates the process section and
the KV-event object for an updated, reviewed template. It retains the template's
compatibility link, process-count authority, selected environment/package keys,
and artifact paths; any shape drift or secret-like environment/argv field is a
bounded failure. Rendered launcher inputs are also exact-allowlisted, so a new
setting must be reviewed explicitly. Generate away from the committed file
first:

```bash
install -d -m 0700 "$HOME/.ctmp"
r114_dir=$(mktemp -d "$HOME/.ctmp/runtime-manifest.XXXXXX")
python3 bench/serving_runtime_image_probe.py \
  --output "${r114_dir}/serving-runtime.json"
cmp compat/deepseek-v4-r34-serving-runtime.json \
  "${r114_dir}/serving-runtime.json"
```

For r34 that generated byte stream is exactly the committed manifest at
SHA-256 `294b3130d696fdcfb2884f9e41bb705e439c63fd7c7c321a764121707af95ff4`
and takes well under one second with a warm image. For a candidate, review the
diff, run `validate-serving-identity-compose.py`, update every manifest pin,
and rerun both service probes before committing. `--replace` is explicit and
accepts only a single regular file in a non-writable parent; it is not a reason
to skip review. Upstream engine-image build integration remains separate.

It also prevents misleading Compose inputs from surviving to an engine roll.
r112 proved that GPU_MEM_UTIL was ignored in favor of the launcher's effective
0.975 default and that the b12x-a16 launcher overwrites a Compose
VLLM_USE_B12X_FP8_GEMM=0 assertion with 1. Canonical Compose now names
GPU_MEMORY_UTILIZATION=0.975 and no longer claims the ineffective FP8 override;
these corrections preserve the already-running effective configuration.

Do not make this 12.5GB image pull part of the ordinary local or Drone loop.
Use node06's or the development host's warm cache. The source tree at
`/opt/vllm/vllm` is not on r34's runtime `sys.path`; only the installed
`site-packages` target above passed the exact-image import.

The operational node06 directory is a mirror rather than a repository
checkout. The repository validator above validates repository-local sources;
it cannot authenticate files after they have been copied to node06. Before an
eventual one-engine trial, first run it locally, then copy the overlay,
middleware, compatibility manifest, and serving-runtime manifest beside the
operational Compose and set these three uncommitted `.env` paths:

```text
MINI_DYNAMO_SERVING_IDENTITY_MIDDLEWARE_SOURCE=./engine_identity_middleware.py
MINI_DYNAMO_SERVING_IDENTITY_MANIFEST_SOURCE=./deepseek-v4-r34.json
MINI_DYNAMO_SERVING_RUNTIME_MANIFEST_SOURCE=./deepseek-v4-r34-serving-runtime.json
```

After transfer, compare every operational byte stream with its local source and
render the operational Compose before acquiring the deployment lock:

```bash
test "$(sha256sum deploy/dspark_0731/docker-compose.compatibility-identity.yaml | cut -d' ' -f1)" = "$(ssh node06 'sha256sum /home/luke/inference/dspark_0731/docker-compose.compatibility-identity.yaml' | cut -d' ' -f1)"
test "$(sha256sum deploy/dspark_0731/engine_identity_middleware.py | cut -d' ' -f1)" = "$(ssh node06 'sha256sum /home/luke/inference/dspark_0731/engine_identity_middleware.py' | cut -d' ' -f1)"
test "$(sha256sum compat/deepseek-v4-r34.json | cut -d' ' -f1)" = "$(ssh node06 'sha256sum /home/luke/inference/dspark_0731/deepseek-v4-r34.json' | cut -d' ' -f1)"
test "$(sha256sum compat/deepseek-v4-r34-serving-runtime.json | cut -d' ' -f1)" = "$(ssh node06 'sha256sum /home/luke/inference/dspark_0731/deepseek-v4-r34-serving-runtime.json' | cut -d' ' -f1)"
ssh node06 'cd /home/luke/inference/dspark_0731 && docker compose -f docker-compose.yaml -f docker-compose.compatibility-identity.yaml config --quiet'
```

Any mismatch or render failure stops the trial. These checks are the
operational-copy gate; do not claim that the repository-relative validator
inspected node06.

The repository validator proves that ordinary Compose has no middleware or identity
mounts, both opt-in engines use the immutable Gilded r34 digest and the image's
real installed Python 3.12 `site-packages` import path, the KV publisher and
sampling floor exactly match their qualified JSON values, every bind is
read-only and cannot create a host path,
both manifest pins, their cross-link, and all mounted artifacts match committed
bytes, and the LB remains in `http` admission mode with the DSpark reliability
guard `off`. It also
validates any `EXTRA_VLLM_ARGS_A_IDENTITY` or
`EXTRA_VLLM_ARGS_B_IDENTITY` override present in the environment; an override
must retain the exact three required arguments.

### Durable DSpark quarantine profile

`docker-compose.dspark-guard-quarantine.yaml` is a separate enforcement
profile. It is deliberately absent from the base and identity-only renders. It
enables compatibility admission and mounts one protected host `/run` directory
read-write so an engine quarantine survives LB crashes and container
recreation. The bounded state document contains only opaque replica, upstream,
and EngineCore SHA-256 commitments; never URLs, raw process identities, prompts,
or responses.

Prepare and validate the fixed host authority, then render the combined profile
without starting anything:

```bash
sudo python3 deploy/dspark_0731/setup_dspark_guard_host.py
sudo python3 deploy/dspark_0731/setup_dspark_guard_host.py --check
python3 deploy/dspark_0731/validate-dspark-guard-compose.py
DSPARK_GUARD_STATE_DIR=/run/mini-dynamo-dspark-guard \
  docker compose -f deploy/dspark_0731/docker-compose.yaml \
    -f deploy/dspark_0731/docker-compose.compatibility-identity.yaml \
    -f deploy/dspark_0731/docker-compose.dspark-guard-quarantine.yaml \
    config --quiet
```

The setup helper exclusively creates the mode-0700 root-owned directory and
mode-0600 canonical state file on `/run` tmpfs, fsyncs both, and otherwise only
validates existing material. The LB holds an exclusive lifetime directory lock,
rejects unsafe, corrupt, noncanonical, or topology-mismatched state at startup,
fsyncs quarantine before publishing the fence, and durably removes the record
before admitting a changed EngineCore. A failed add or removal remains fenced
and is exported as `persistence_failure`. The state is marked runtime-dirty
before startup. An unclean exit or poisoned mutation therefore makes the next
LB fence every unresolved replica and durably quarantine its then-attested
EngineCore; only a clean, fully resolved shutdown clears that marker.

This is an admission artifact, not authorization to roll node06. Keep it off
until cooling is repaired, identity admission passes a rolling one-engine
qualification, and guard `observe` proves the exact live r34 counter/label
shape and false-positive rate first on one TP4 pair and then both pairs. Every
operational recreate must still hold the common deployment lock.

Do not apply this overlay to both engines together. First verify the pinned
image still supports vLLM's `--middleware` import contract, single-home
production on the peer, and recreate only the candidate engine. Test the direct
endpoint with the existing engine bearer, then run the deterministic agent
smoke and performance scout. Roll the second engine only after the candidate is
clean. The identity-only profile must keep
`MD_UPSTREAM_ADMISSION_MODE=http`; only the separately rendered durable guard
profile may request compatibility admission after all its prerequisites pass.
The middleware verifies the live vLLM distribution, configured model
name/context, and tokenizer bytes. It still re-publishes the manifest's model
root and renderer profile only after the real initialized `/v1/models` and all
ten renderer/token-ID goldens match. Compose—not the process—still provides the
immutable image binding. The health bracket and schema-v3 response bind the
frontend to the exact stable EngineCore child, live typed KV configuration, and
owned event/replay listening sockets. Before publishing that response, it also
matches the full normalized argv, allow-listed environment, package versions,
and exact launcher/NCCL artifacts from runtime-manifest schema v2. The response
contains only their four digests, not raw argv or environment. This does not
prove publisher-thread
liveness, event advancement, retained sequence zero, or complete and timely
replay; those require live node06 qualification. It also does not attest the
complete live driver/kernel/topology, persistent-cache mount, or warmup bundle
required to close issue #15. The image's cache namespace points under
/cache/jit, while base Compose still mounts its historical host cache at
/root/.cache; persistence must be preseeded and qualified on one engine before
changing that mount. A later qualified runtime-bundle publisher must close those bindings
before compatibility admission can be enabled.

## Offline dual-companion security harness

`docker-compose.snapshot-companion-offline.yaml` is a standalone,
profile-disabled fixture contract. It is not merged with production Compose
and its reserved `.invalid` images cannot start by default. A normal render
selects zero services; the explicit `snapshot-companion-offline` profile
selects two independent pairs, one per engine.

Each pair has its own companion UID, tmpfs runtime bind, Unix socket, 32-byte
session secret, fixture path, client process, and healthcheck. Neither pair can
mount, authenticate, probe, or name the other's socket. Both client fixtures
use the future LB UID `12002`; engine A's companion uses UID `12001` and engine
B's uses `12003`. Only the numeric GID `12000` is common; fixture directories
are also per-engine and read-only.

```bash
SNAPSHOT_RUNTIME_DIR_A=/run/mini-dynamo-snapshot-offline-a \
SNAPSHOT_RUNTIME_DIR_B=/run/mini-dynamo-snapshot-offline-b \
SNAPSHOT_SESSION_SECRET_FILE_A=/run/secrets/mini-dynamo-snapshot-session-a \
SNAPSHOT_SESSION_SECRET_FILE_B=/run/secrets/mini-dynamo-snapshot-session-b \
  deploy/dspark_0731/validate-snapshot-companion-host.sh

python3 deploy/dspark_0731/validate-snapshot-companion-compose.py
```

The host preflight requires distinct, symlink-free tmpfs directories owned
`12001:12000` and `12003:12000` at mode `0750`, plus distinct root-owned secret
inodes at `0:12000`, mode `0440`, one link, and exactly 32 bytes. The Compose
validator rejects host networking/IPC/PID access, GPUs/devices, Docker socket
mounts, broad host mounts, cross-engine authority mounts, service dependencies,
writable client roots, or healthchecks that address the peer socket. This is a
static/offline deployment gate; it does not claim that fixture executables or
production wiring exist.

The standalone companion also supports a metrics-only Unix endpoint, but this
offline Compose fixture deliberately does not enable it yet. Production-shaped
wiring must set exactly `MD_SNAPSHOT_METRICS_SOCKET_PATH` and
`MD_SNAPSHOT_METRICS_GROUP_GID` (not `MD_SNAPSHOT_METRICS_BIND`), mount a
different companion-owned parent from the snapshot socket, and prepare it as a
symlink-free setgid directory that is not writable by group or other. The
metrics group must differ from GID `12000`; only the scraper/Caddy identity may
join it. The snapshot parent must also become setgid so its future socket cannot
inherit the metrics group through the companion process. Startup verifies
parent ownership, group separation and inheritance,
publishes mode `0660` without replacing an existing pathname, and cleans only
the inode it published. Add those mounts, the scraper route, and validator
assertions together; a code-capable UDS alone is not permission to alter
node06.

## Offline engine-incarnation provisioning

`mini-dynamo-attestation-provisioner` closes the host attestation-writing
boundary without receiving Docker access. It is a one-shot binary: first an
independently privileged capture step writes the existing schema-v1
`node06_engine_metadata.sh` result, then the provisioner reads only that
explicit protected file and the digest secret. It accepts no command-line
arguments and is silent on success; failures print only a bounded reason.

Build it on the development host. Do not compile it on node06 while the engines
are resident:

```bash
cargo build --release --locked --bin mini-dynamo-attestation-provisioner
```

The future per-engine service-manager unit should run the equivalent of the
following after a fresh metadata capture. These paths are examples of the
required isolated authority domains, not production Compose wiring:

```bash
# Privileged capture owner; output is mode 0600 and is never mounted in the LB.
bench/node06_engine_metadata.sh \
  /run/mini-dynamo-snapshot-a/engine-metadata.json dspark-0731

MD_SNAPSHOT_ENGINE_METADATA_PATH=/run/mini-dynamo-snapshot-a/engine-metadata.json \
MD_SNAPSHOT_DIGEST_SECRET_PATH=/run/secrets/mini-dynamo-snapshot-digest-a \
MD_SNAPSHOT_ATTESTATION_PATH=/run/secrets/mini-dynamo-snapshot-attestation-a \
MD_SNAPSHOT_SECRET_OWNER_UID=0 \
MD_SNAPSHOT_SECRET_GROUP_GID=12000 \
  /usr/local/libexec/mini-dynamo-attestation-provisioner
```

The default capture freshness limit is 30 seconds and can be reduced or raised
to at most five minutes with `MD_SNAPSHOT_ATTESTATION_MAX_AGE_MS`. The metadata
must be an owner-only, singly linked, regular mode-`0600` file below trusted
ancestors. If it carries a qualification receipt, the receipt must be verified
and have `qualified` status. The provisioner canonicalizes an allow-listed
immutable identity, including the vLLM process start derived from `/proc` by the
capture helper. It intentionally excludes capture time and binds the result to
the digest secret using the companion's existing envelope format.

The output parent is locked during publication. A same-directory random
temporary file is written and fsynced, assigned the requested owner/group and
exact mode `0440`, atomically renamed, followed by a directory fsync and an
authenticated read-back. Existing outputs are never silently repaired: an
invalid envelope, unsafe inode, older process start, or different evidence for
the same process fails closed and preserves the current file. Run a fresh
capture/provision step after every engine process replacement; a companion's
existing watcher will fence authority until the new authenticated identity is
available.

## Production-shaped snapshot overlay

`docker-compose.snapshot-companion.yaml` joins the two standalone companions
and the one-shot attestation provisioners to the canonical stack without
granting Docker, GPU, host IPC, privileged, or host network access. It is not
part of an ordinary `docker-compose.yaml` deploy. The two companions require
the explicit `snapshot-companion` profile, and the short-lived root
provisioners require the separate `snapshot-attestation` profile.

The LB-side snapshot clients live in a **separate**
`docker-compose.snapshot-lb.yaml`. That split is the #157 boundary: the
companion overlay alone never modifies `ds4-loadbalancer`, so it cannot couple
the production serving path to `/run` tmpfs state. Applying the LB overlay is
the reviewable event that does create that coupling, and it requires the
boot-time authority units under `systemd/` (#158) — without them a reboot
wipes `/run`, runc cannot create the container, and because that is a
create-time failure rather than a process exit, `restart: unless-stopped`
never retries it. That is exactly the ~50-minute #156 outage.

Snapshot routing defaults to `off` even with both overlays; only an explicit
`MD_SNAPSHOT_ROUTE_MODE=shadow` enables both the exact-router gate and
snapshot authority in lockstep so the inventories can be consumed, and the
pinned LB also enforces that compact inventories cannot select placement.

Render the complete contract without starting it:

```bash
python3 deploy/dspark_0731/validate-snapshot-production-compose.py
docker compose -f deploy/dspark_0731/docker-compose.yaml \
  -f deploy/dspark_0731/docker-compose.snapshot-companion.yaml \
  -f deploy/dspark_0731/docker-compose.snapshot-lb.yaml \
  --profile snapshot-companion --profile snapshot-attestation config --quiet
```

The validator checks immutable images, numeric SO_PEERCRED identities, exact
per-engine endpoints and authority mounts, raw-event/snapshot mutual
exclusion, read-only roots, dropped capabilities, no devices or published
ports, isolated session/digest/attestation domains, and dedicated metrics-only
groups. It also validates `Caddyfile.snapshot-companion`: Caddy can reach only
`/metrics/snapshot/0|1` over the two metrics UDS paths and must never join
session GID `12000`.

Two of its checks exist specifically to prevent a #156 recurrence:

- **Serving-path isolation** — the base stack, and the base stack plus the
  companion overlay, must both render an `ds4-loadbalancer` with zero bind
  mounts on a volatile filesystem and no changed runtime identity. A reboot
  that wipes `/run` therefore cannot stop the LB from being created.
- **Boot authority** — once the LB overlay *is* applied, every `/run` mount it
  adds must have a parent in `systemd/tmpfiles.d/mini-dynamo-snapshot.conf`,
  and `mini-dynamo-snapshot-authority.service` must be ordered
  `Before=docker.service` while being pulled in by `WantedBy=`, never
  `RequiredBy=`. Ordering is guaranteed; a provisioner failure must leave the
  companions down without ever blocking the serving path.

Before provisioning, create every host authority on `/run`, capture both engine
metadata files, and run the same command below with a trailing
`pre-provision`. That mode validates the attestation output directories but
does not require their not-yet-created `engine.json` files. Run the two
`snapshot-attestation` services, then rerun without an argument for the full
gate before starting either companion:

```bash
sudo python3 deploy/dspark_0731/setup_snapshot_production_host.py
sudo python3 deploy/dspark_0731/setup_snapshot_production_host.py --check

SNAPSHOT_RUNTIME_DIR_A=/run/mini-dynamo-snapshot-a \
SNAPSHOT_RUNTIME_DIR_B=/run/mini-dynamo-snapshot-b \
SNAPSHOT_METRICS_DIR_A=/run/mini-dynamo-snapshot-metrics-a \
SNAPSHOT_METRICS_DIR_B=/run/mini-dynamo-snapshot-metrics-b \
SNAPSHOT_SESSION_SECRET_FILE_A=/run/secrets/mini-dynamo-snapshot-session-a \
SNAPSHOT_SESSION_SECRET_FILE_B=/run/secrets/mini-dynamo-snapshot-session-b \
SNAPSHOT_DIGEST_SECRET_FILE_A=/run/secrets/mini-dynamo-snapshot-digest-a \
SNAPSHOT_DIGEST_SECRET_FILE_B=/run/secrets/mini-dynamo-snapshot-digest-b \
SNAPSHOT_ATTESTATION_DIR_A=/run/mini-dynamo-snapshot-attestation-a \
SNAPSHOT_ATTESTATION_DIR_B=/run/mini-dynamo-snapshot-attestation-b \
SNAPSHOT_ENGINE_METADATA_FILE_A=/run/mini-dynamo-engine-metadata-a.json \
SNAPSHOT_ENGINE_METADATA_FILE_B=/run/mini-dynamo-engine-metadata-b.json \
  deploy/dspark_0731/validate-snapshot-production-host.sh
```

The setup helper has no production path or numeric-identity overrides. It
creates groups `12000`, `12004`, and `12005`; non-login service users
`12001`, `12002`, and `12003`; the six exact setgid tmpfs directories; and four
independent 32-byte secrets. It never overwrites a secret or repairs an unsafe
existing identity/path. Name/ID collisions, symlinks, non-tmpfs authority,
unexpected ownership/mode/link count, duplicate secret contents, and unsafe
existing metadata or attestation outputs all fail before ordinary setup
mutation. A partially completed safe first run can be rerun; `--check` is
read-only and requires the managed identities, directories, and secrets.

Metadata target files are deliberately not created: the helper validates their
root-owned tmpfs parent and any existing target, then
`bench/node06_engine_metadata.sh` creates the fresh mode-`0600` content. The
attestation provisioners similarly own `engine.json` publication.

Caddy membership is never changed by the default command. Only immediately
before installing `Caddyfile.snapshot-companion`, opt in explicitly and rerun
the read-only check:

```bash
sudo python3 deploy/dspark_0731/setup_snapshot_production_host.py \
  --configure-caddy
sudo python3 deploy/dspark_0731/setup_snapshot_production_host.py \
  --check --configure-caddy
```

That grants the existing `caddy` identity only metrics GIDs `12004` and
`12005`, preserves its other groups, and refuses to proceed if Caddy has either
primary or supplementary membership in session GID `12000`. Use
`--caddy-user NAME` only with `--configure-caddy` when the service account has
a different explicit name. Restart the Caddy service after this explicit
membership change so its long-lived process receives the new supplementary
groups; no restart is required when running the default setup command.

With the same exported paths, the bounded order is:

```bash
deploy/dspark_0731/validate-snapshot-production-host.sh pre-provision
docker compose -f deploy/dspark_0731/docker-compose.yaml \
  -f deploy/dspark_0731/docker-compose.snapshot-companion.yaml \
  --profile snapshot-attestation run --rm snapshot-attestation-a
docker compose -f deploy/dspark_0731/docker-compose.yaml \
  -f deploy/dspark_0731/docker-compose.snapshot-companion.yaml \
  --profile snapshot-attestation run --rm snapshot-attestation-b
deploy/dspark_0731/validate-snapshot-production-host.sh full
```

Snapshot parents must be companion-owned `2750` directories in session GID
`12000`; metrics parents must be companion-owned `2750` directories in GIDs
`12004` and `12005`; secrets are distinct root-owned 32-byte `0440` files;
metadata is root-owned `0600`; provisioned attestations are root/session-group
`0440`. All directories are distinct, symlink-free tmpfs paths and every
authority inode is unique.

The overlay is an admission artifact, not a production enablement. Do not copy
the Caddy snippet or start either profile until current images are repinned and
both the setup helper's `--check` and the host validator pass on node06. The
first rollout
must keep `MD_SNAPSHOT_ROUTE_MODE=off` (which also forces the exact-router
gate off); enable `shadow` only after both
companions are ready, then preserve ordinary approximate serving throughout.

### One-command readiness and LB-recovery gate

Copy the repository-owned gate beside the node06 Compose files after updating
it. Its default mode is read-only and is safe to run while either source is
fenced:

```bash
scp bench/snapshot_recovery_gate.py \
  node06:/home/luke/inference/dspark_0731/
ssh node06 'cd /home/luke/inference/dspark_0731 && \
  python3 snapshot_recovery_gate.py \
    --output /tmp/snapshot-readiness-$(date +%s).json'
```

Exit code `0` means both sources are authoritative and stable; exit code `3`
means at least one source is not ready and **nothing was recreated**. Any other
nonzero code is a failed precondition. Each run first executes the host setup
check, full host validator, semantic Compose validator, profile render,
immutable-image check, current-container health check, and rollback-config hash
comparison. It samples each metrics UDS twice so a reconnecting source cannot
slip through as ready.

Only after the read-only run returns `0`, run the explicit mutation mode:

```bash
ssh node06 'cd /home/luke/inference/dspark_0731 && \
  python3 snapshot_recovery_gate.py --apply \
    --output /tmp/snapshot-recovery-$(date +%s).json'
```

Apply mode owns `/run/lock/mini-dynamo-node06-deployment.lock` for the complete
inspect/mutate/measure/rollback interval. It defaults to five LB-only shadow
recreates, requires both LB snapshot inventories authoritative on every pass,
checks that engine identities and restart counts never move, and applies the
three-second nearest-rank p95 gate. Whether the timing gate passes or fails, it
force-recreates the exact preflight base service and verifies its original
Compose config hash, image, 2/2 health, and snapshot-off state before releasing
the lock. It never starts, stops, clears, or restarts an engine or companion.

The mode-`0600` JSON journal is reserved before mutation and contains only
public image/config identities, bounded readiness state, aggregate block/token
counts, timings, and rollback status. It never records commands, environment,
credentials, socket paths, prompts, token IDs, responses, or logs. Use a fresh
output path for every run; the gate never overwrites evidence.

### Detached exact-route shadow-soak gate

The 104-source/100K compact-index qualification has a separate deployment
owner, `bench/node06_shadow_soak_gate.py`, and now refuses to start unless it is
inside `bench/node06_gpu_guard.py`. Copy
`bench/node06_operational_moratorium.py` beside the guard whenever these scripts
are staged. Launch the pair as a detached transient
systemd service using the command in `AGENTS.md`; do not attach the workload and
rollback lifetime to SSH. The outer guard observes all eight GPUs, admits a
65C-or-cooler start, aborts at 78C or on lost telemetry, and owns a separate
mode-0600 append-only JSONL journal. It fsyncs a start record, periodic
checkpoints, and the final result; records contain stable GPU names and hashed
UUID identities plus bounded aggregates, but never the child command or
environment. Stdout/journald receives only run ID, status, reason, and exit
code. A launch-gated exec shim arms parent-death cancellation before
releasing the command and supervises escaped sessions, so loss of the outer
sampler cannot leave a direct benchmark running. On abort it gives
request-generating descendants at most five seconds to cancel, escalating on
another hot or lost sample, before KILL while the inner deployment owner
retains a separate 780-second rollback grace. Telemetry continues while
available; loss kills request work without preempting bounded rollback. This is a last-resort
request-generator stop, not proof of healthy passive cooling or absence of
thermal slowdown; chassis airflow/coolant/inlet and BMC/driver slowdown
monitoring remain independent. The rollback-owner exception is explicit and
must not be used for direct or candidate request roots. The inner gate admits
only an explicitly named immutable current baseline and local `sha256:` candidate, recreates only
`ds4-loadbalancer`, and holds the common deployment lock until the exact
snapshot-shadow/soak-off baseline has been restored and verified. Both engine
and companion identities must remain unchanged.

The child retries only the two proxy-signed pre-dispatch outcomes
`tokenizer_unavailable` and `attestation_changed`. Generic 503s, upstream 503s,
and no-healthy-upstream responses fail immediately. A passing journal also
requires the bounded client retry reasons and LB source-attempt counters to
match exactly, so retry cannot conceal a serving failure. The gate reads the
mode-0600 `.env` itself and never places the bearer in argv, systemd properties,
or its content-free journal.

The 2026-08-14 host loss occurred during the third consecutive long-context
capacity/soak campaign after completed 711.81s and 1,801.80s all-eight-GPU
cells. No thermal telemetry was captured, so causation is unknown. After the
cooling repair, first run `bench/capture_node06.sh` to record idle per-GPU
temperature/power plus the device-reported target, maximum-operating, slowdown,
and shutdown thresholds, then run only one isolated TP4 pair under the guard.
A dual-pair cell is admissible only after that controlled soak stays below the
operational ceiling and independent BMC/driver evidence shows no slowdown; the
core-temperature guard alone cannot establish that. Candidate container
startup, model load, and JIT occur before the request gate and require one-TP4
isolation plus a manual BMC/facility and driver watch until a container-aware
rollout owner exists. The 52/64-app boundary remains last, not first.
