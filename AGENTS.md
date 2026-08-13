# AGENTS.md — working on mini-dynamo

Guidance for coding agents (and humans) developing and testing mini-dynamo.
The production deployment is the DeepSeek-V4-Flash serving stack on **node06**;
full experiments require GPUs, so they run there, not locally.

## Local (no GPUs needed)

### Fast iteration contract

Do not run the complete release gate after every edit. Use the narrowest test
that proves the code being changed, then widen once before publishing:

```bash
# inner loop (normally 2-5s when warm)
cargo fmt --check
cargo test --locked <module-or-test-name>
cargo check --locked

# pre-push gate (run once after focused tests are green)
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
python3 bench/agentbench.py validate
python3 -m unittest discover -s bench -p 'test_*.py'
```

On the 2026-08-12 development checkout, a focused/warm test took about 2-3s,
all 104 Rust tests took about 3s including the crate rebuild, and the thin-LTO
release relink took about 19s. If a routine loop is materially slower, inspect
cache misses, downloads, disk pressure, or accidental all-target/release work
before waiting through repeated slow builds. Use `/usr/bin/time` or the timing
printed by repository scripts; record build/transfer/benchmark wall time in an
experiment entry whenever workflow speed itself changes.

The development host's `/tmp` is a shared 31GiB tmpfs and can be full even when
the main filesystem has hundreds of GiB free. Put Rust worktrees on the
disk-backed home filesystem. If an existing temporary worktree must be used,
reuse the canonical checkout's warm target and move compiler scratch to disk:

```bash
mkdir -p /home/karolis/.cache/mini-dynamo-tmp
CARGO_TARGET_DIR=/home/karolis/go/src/github.com/helixml/mini-dynamo/target \
TMPDIR=/home/karolis/.cache/mini-dynamo-tmp cargo test --locked
```

Do not launch two Rust gates concurrently against that shared target; fan out
Python and Compose beside one Rust lane instead. Check both `df -h /tmp` and target
size before a cold build. Clean only build artifacts created by this project;
unrelated `/tmp` worktrees and caches belong to other active tasks.

### Fast image build and transfer

Compile on the development machine in the pinned Bookworm builder, not on the
GPU host. node06 normally has little free host RAM while both vLLM engines are
resident. The persistent BuildKit target/registry caches make unchanged image
builds about 2-3s; a Rust source edit normally pays only the crate's release
relink. The resulting runtime image is about 14MB and transferred over
Tailscale in about 4s in the qualified r23 run.

```bash
# One-time builder creation if it is absent.
docker buildx create --name mini-dynamo-publisher \
  --driver docker-container --use

# Build only, or build and stream into node06's Docker image store.
bench/build_transfer.sh rust-rNN-description-$(git rev-parse --short HEAD)
bench/build_transfer.sh rust-rNN-description-$(git rev-parse --short HEAD) --node06
```

The cold first build still downloads the Rust base and compiles dependencies;
judge the workflow by its warm edit/rebuild time. A dependency change is a
legitimate one-time cold build. Drone fetches dependencies once, then fans
Clippy and Rust tests into isolated target directories while Python and
Compose run beside them. Build #205 completed the full PR gate in 58s versus
69s for the prior serial Rust lane. If a no-dependency change regresses that
loop, inspect the step timings and cache/export path before accepting it.

Drone 2.12 silently drops unsupported `when.paths` fields. Do not use them.
After `cargo fetch`, the normal Rust step runs `bench/drone_publish_plan.sh`
while `.git` is usable. On pushes it validates exact 40-hex revisions, verifies
workspace HEAD equals `DRONE_COMMIT_SHA`, fetches a missing shallow predecessor
by exact object ID, and atomically replaces `.drone-publish-plan` with private
per-image markers. On pull requests it does no planning. Every GHCR step calls
`bench/drone_publish_guard.sh`, which requires only its revision-bound marker
and never Git, before starting Kaniko. A missing marker skips the step
successfully before registry login or image construction; a
missing/invalid/unfetchable revision, unsafe marker, or empty changeset fails
closed. Keep the path matrix and pipeline wrapper tests current whenever image
inputs change.

The expensive locked Cargo graph lives in the public, source-free image named
by `.docker/rust-deps-key`. The dependency guard admits only `Cargo.toml`,
`Cargo.lock`, `rust-toolchain.toml`, `Dockerfile.deps`, or its derived key. Both
application publishers wait for that guard step, so a dependency change seeds
the image before either app build. Source-only releases inherit the existing
content-keyed image and compile offline. `Dockerfile.deps` keeps the locked
registry fetch in a separate Kaniko-cacheable layer before compiling the seed,
so a recipe-only rebuild can reuse downloads. CI/docs/bench/deploy-only pushes still
instantiate three cheap guards on this server but must perform zero Docker or
registry work.

After changing a dependency input, update every content-keyed reference and
validate it before building:

```bash
python3 bench/rust_deps_image.py --update
python3 bench/rust_deps_image.py
```

For the first local build of a new key, seed the base explicitly; after Drone
publishes it on `main`, ordinary builds pull the same public default:

```bash
deps_ref=$(python3 bench/rust_deps_image.py --print-reference)
docker build -f Dockerfile.deps -t "$deps_ref" .
docker build --build-arg RUST_DEPS_IMAGE="$deps_ref" .
```

`Cargo.toml` deliberately includes Rust sources, examples, compatibility
fixtures, manifests, and the public README/CHANGELOG/RELEASE/LICENSE documents.
The release Dockerfiles still copy only manifests plus `src/` and, for the LB,
`compat/`, so benchmark and operational-document edits do not invalidate the
container's thin-LTO build layer. If an unrelated edit unexpectedly triggers an
18–20s relink, inspect both `cargo package --allow-dirty --list` and Docker
`COPY` inputs; do not paper over it with another target directory.

The router is the interesting surface — `src/router.rs` contains the active
Rust tests and frozen legacy fingerprint goldens. Add a Rust test for every
routing change and retain a golden wherever compatibility matters.

For compact-index work, keep the fast GPU-free correctness/performance loop
separate from node06:

```bash
cargo test --locked --test digest_exact_parity
cargo test --locked digest_index
cargo run --release --locked --example digest_index_bench
BENCH_MODE=exact-rss target/release/examples/digest_index_bench
BENCH_MODE=digest-rss target/release/examples/digest_index_bench
```

The digest index is a memory/recovery optimization, not a faster lookup path.
Its pre-shadow gates are: no overclaims in differential tests, matched 80K
lookup at most 250us and 5x raw exact, 524K at most 2ms, 36,612-record import
at most 100ms, and steady index RSS at most 60% of raw exact. Do not deploy a
companion merely because these local gates pass: authenticated UDS session,
incarnation/watermark freshness, gap fencing, and atomic tail catch-up remain
mandatory.

Never interpret vLLM's event sequence as a dense counter. It advances with
scheduler steps while only steps emitting KV events are retained. Snapshot
tail transport therefore needs its own contiguous authenticated delivery
sequence while preserving the last real-event sequence as the authoritative
watermark. A numeric gap in real vLLM event sequences is not data loss.

For snapshot transport work, keep the warm inner loop to the module under
change; these focused tests complete in a few seconds and do not need node06:

```bash
cargo test --locked snapshot_transport
cargo test --locked snapshot_tail_wire
cargo test --locked snapshot_tail
cargo test --locked snapshot_secret_file
cargo test --locked snapshot_socket_path
cargo test --locked snapshot_actor
cargo test --locked snapshot_supervisor
cargo test --locked snapshot_digest_delta
cargo test --locked snapshot_consumer
cargo test --locked --test snapshot_consumer_adversarial
cargo test --locked snapshot_producer
cargo test --locked snapshot_reconnect
cargo test --locked companion_runtime
cargo test --locked companion_index_source
cargo test --locked --test snapshot_stack_e2e
cargo test --locked --test snapshot_digest_lifecycle
```

The one-shot exchange intentionally owns one Unix stream and reads its response
to EOF. Do not bolt tail frames onto that stream: the long-lived companion actor
gets a separate authenticated tail connection, bounded queue, and deadline.
Socket path creation/removal belongs to the companion-owned listener lifecycle,
not the generic transport helper; never unlink a path supplied by an untrusted
or LB-writable directory.

Only `snapshot_bootstrap` may construct a prepared generation. Keep that token
opaque: the actor must not accept a separately asserted reset scope, watermark,
identity, lifecycle, or caller-built index. A same-identity replacement stays
private until authenticated caught-up and preserves the current publication;
an identity/key/generation change or owner-session failure revokes immediately.

Keep the two Unix roles separate. `snapshot_supervisor` is companion/server
admission for accepted streams. `snapshot_consumer` is the LB/client protocol
over an already-connected stream; a future outbound reconnect owner supplies a
fresh non-reused challenge and the single absolute deadline. Dropping that
consumer future must synchronously fence its actor epoch and signal any bounded
blocking snapshot build to cancel.

`snapshot_producer` is the engine-neutral companion/server half accepted by the
supervisor. Its source callback must subscribe live before building a snapshot,
return owned state, observe cancellation, and never retain engine/global locks
across serialization or socket writes. Tail delivery is bounded and applies
backpressure; a dropped LB client must cancel source work immediately. The
producer owns one absolute snapshot-phase timeout covering hello, source build,
authentication, and snapshot write. Once the snapshot is written, dequeuing a
tail event starts a fresh bounded write budget and each successful write starts
a fresh tail-wait budget. A progressing tail may therefore outlive the snapshot
timeout, but an idle source or blocked writer cannot retain a session forever.
Source revocation, disconnect, and supervisor shutdown stay higher priority
than either timer. Do not restore a single absolute supervisor lifetime for
handler-managed sessions: total resources remain bounded by two admitted
clients, entry/byte queue caps, frame limits, and the phase/idle budgets.

`companion_runtime` is library-only and off by default. It must validate and
load all state before binding, bind the public socket last, clear readiness and
inode-check-clean the socket on every exit, and bound supervisor drain on
shutdown. Until the authenticated hello carries an engine selector, fail closed
unless exactly one source is configured; never guess which engine a client
intended. Keep socket publication and exact-source authority separate in
telemetry: operational `ready` is their conjunction, while `listening` and
`source_ready` explain which side is missing. A bound socket alone must never
claim exact readiness, and stopping the listener must clear operational ready.

`companion_index_source` owns one long-lived per-engine digest index independently
of LB sessions. Register a session before cloning its snapshot boundary, do
traversal/encoding off the ingestion lock, retain only bounded qualified wire
payloads for active tails, and fence every session plus the generation when the
transport owner loses authority, the index fails, or attested incarnation
changes. vLLM event watermarks are sparse: accept strictly increasing forward
jumps here and let the process-level replay fence distinguish omitted scheduler
steps from lost event batches. A client disconnect removes only that subscriber;
it must never stop or clear the index. Tail queues are bounded by both entries
and bytes; payloads are shared `Bytes`, and overflow or a source fence must use
the out-of-band revocation signal rather than drain stale FIFO events.

`companion_index_owner` owns one subscribed ZMQ connection per engine. After
startup, disconnect, or incarnation loss it must stay fenced until a fresh live
batch supplies a current watermark; the last pre-disconnect watermark is never
an authoritative recovery boundary. Subscribe first, then stream a complete
replay from zero through that fresh watermark into private source state. Treat
an incremental gap the same way until the source has a transactional bounded
gap stage: do not retain `replay_limit * max_payload_bytes` decoded batches or
publish a partially validated gap. Generation-guard every blocking replay fold,
make repeated connect failures and already-created clean stages idempotent, and
keep observer events closed and content-free. If the current watermark or a
live gap exceeds the replay limit, keep the installed SUB connection in stable
fenced observe-only mode. Do not reconnect on each subsequent event: only an
authoritative all-blocks clear or an attested incarnation change may restore
trust without a complete replay from zero.

`mini-dynamo-snapshot-companion` is the standalone one-engine composition. It
must remain a separate binary and `Dockerfile.companion`; build the ordinary LB
with `--bin mini-dynamo` so companion changes do not add a second release relink
to the serving-image loop. Serve mode must preflight the session secret, digest
secret, and authenticated incarnation file before any TCP bind, ZMQ connect, or
UDS publication. Invalid attestation refreshes immediately remove authority;
logs and metrics expose only closed reason labels. The container publishes no
metrics port: loopback inside a bridge container is not host-scrapable. For
production, select `DS4_SNAPSHOT_METRICS_SOCKET_PATH` and its dedicated
non-root `DS4_SNAPSHOT_METRICS_GROUP_GID`; setting that socket together with
`DS4_SNAPSHOT_METRICS_BIND` is an error. The metrics parent must be a distinct
companion-owned, symlink-free, non-writable setgid directory whose group is
different from the snapshot/session parent group. The publisher verifies that
both authority directories are setgid and group-traversable, that the mode-0660
metrics socket inherited the configured group, and removes only its own inode.
Put Caddy/Prometheus in only that metrics group, never in the
snapshot/session authority group. The production-shaped overlay, Caddy snippet,
and host/Compose validators now exist under `deploy/dspark_0731`; they remain
admission artifacts until hot LB attestation refresh lands. Always render both
profiles and run both production validators
before touching node06, first start with snapshot routing off, and repin current
immutable images rather than treating the committed candidates as evergreen.

`mini-dynamo-attestation-provisioner` is a host-side, one-shot control-plane
binary. It accepts no arguments and obtains only protected file paths, numeric
ownership, and a freshness bound from the environment. Its identity source is
schema-v1 output from `bench/node06_engine_metadata.sh`; never derive identity
inside the provisioner by granting it Docker access. Keep that input owner-only
mode `0600` and fresh (30s default), and keep the digest secret out of argv and
logs. Publication must remain a same-directory fsynced temporary file, exact
owner/group/mode `0440`, atomic rename, and directory fsync under the parent
lock. A corrupt existing output, older process start, or changed evidence for
the same process must fail closed without replacement. Do not turn transient
capture metadata such as capture time into incarnation identity.

`snapshot_reconnect` is the LB-side owner around the consumer. Normal attempts
are serial; only an explicit bounded replacement may overlap a second session.
Validate the trusted socket parent on every connect, use a fresh OS-random
challenge under the bounded reuse ledger, and carry one absolute attempt
deadline through connect and consumption. Shutdown drops the consumer future
immediately; approximate serving is never owned by this path. Its hot
attestation channel carries a monotonic authority revision in addition to the
optional incarnation. Any unavailable, changed, closed, or skipped authority
revision must revoke actor state before dropping the active session; do not
replace it with a value-only watch that can coalesce an invalid interval.

LB consumption is separately gated by `DS4_SNAPSHOT_ROUTE_MODE=shadow`. Keep
its per-upstream socket, companion UID, session secret, digest secret,
attestation, and group lists at exact upstream cardinality and preflight all
authorities before spawning any reconnect task. Direct `DS4_KV_EVENT_MODE`
authority and snapshot authority must remain mutually exclusive. Snapshot
inventory is shadow-only until the recorded comparison gate is met: never let
this mode select placement, affect upstream health, or make `/health` fail.
The initial attestation is still preflighted before any task starts. Once
running, `DS4_SNAPSHOT_ROUTE_ATTESTATION_REFRESH_MS` reloads it through the
same hardened file/MAC policy. Same identity is a no-op; invalid authority
suppresses reconnects, and a valid atomic rotation reconnects with the fresh
expected identity without restarting the stateless LB.

Keep the true public-stack harness green: it composes safe socket publication,
supervisor, producer, reconnect owner, consumer, actor, and digest index. Use
`cargo run --release --locked --example snapshot_shape_bench` for the captured
36,612-block authenticated wire/rebuild measurement; timings are observational
and must not become flaky CI thresholds.

## node06 — the test/production box

node06 is a Tailscale host running two vLLM+DSpark TP4 instances behind this
LB. Connect with the SSH alias (config already set up):

```bash
ssh node06            # root@100.89.187.17 via Tailscale
```

Layout on the box:

- `/home/luke/inference/dspark_0731/` — the whole serving stack (compose:
  `ds4-loadbalancer` + `dspark-0731` + `dspark-0731-b`), plus `.env`
  (`VLLM_API_KEY=<caddy bearer>`, mode 0600) and the bench scripts.
- `deploy/dspark_0731/docker-compose.yaml` in this repository is the
  canonical Compose source. The infra repository and node06 file are mirrors;
  edit here first and use `sync-compose.sh` to update the infra copy.
- Ports (loopback): LB API `:8006`, LB metrics `:8007`, engines `:8012`/`:8013`.
- The bearer token used by clients AND for engine probes:
  `grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile | head -1 | cut -d' ' -f2`
  (never hard-code it; it is not committed anywhere).

### Build + deploy a new LB image

Prefer the local cached build/transfer path above. It avoids CPU, RAM, disk, and
dependency-download pressure on the live GPU box. The old source-build path is
an emergency fallback only:

```bash
# ship source, build the image tag on node06
tar czf /tmp/md.tgz --exclude=.git --exclude=target . && scp /tmp/md.tgz node06:/tmp/
ssh node06 'rm -rf /tmp/md && mkdir /tmp/md && tar xzf /tmp/md.tgz -C /tmp/md \
  && cd /tmp/md && docker build -t ghcr.io/helixml/ds4-loadbalancer:<tag> .'

# swap the LB (engines untouched; ~4s, LB-only)
ssh node06 'cd /home/luke/inference/dspark_0731 \
  && flock --nonblock /run/lock/mini-dynamo-node06-deployment.lock \
    env LB_IMAGE=ghcr.io/helixml/ds4-loadbalancer:<tag> \
    docker compose up -d ds4-loadbalancer'

# verify
ssh node06 'docker logs ds4-loadbalancer --tail 1; curl -s :8007/metrics | grep ds4proxy_upstream_up'
```

Rollback is the same command with the previously accepted immutable
`LB_IMAGE=repository:tag@sha256:digest`, recorded in the experiment/deployment
journal. Never roll back through a mutable tag alone. The LB is stateless;
swapping it never touches the engines or their KV caches.

Every tool that recreates `ds4-loadbalancer` must hold the same exclusive
`/run/lock/mini-dynamo-node06-deployment.lock` for its complete inspect/mutate/
verify interval. The Phase-B P2P harness also adds a unique ownership label and
service hash; it will not restore over a container it no longer owns. Do not
bypass either fence with a raw concurrent `docker compose up`.

### Preflight engine flags before a rolling restart

Do not discover an unsupported vLLM flag combination by restarting a resident
engine. First exercise the pinned image's argument validation on the healthy
peer (or in a disposable container for a new image). For example:

```bash
ssh node06 'docker exec dspark-0731 python -c "from vllm.engine.arg_utils import EngineArgs; EngineArgs(max_num_partial_prefills=2, max_long_partial_prefills=1)._check_feature_supported()"'
```

Translate the proposed CLI values into `EngineArgs` keyword arguments and
require exit zero before touching compose. Also check `vllm serve --help` when
adding a newly introduced flag. This private validation hook is deliberately
pinned-image-specific: it took 16.5s to reject r34 concurrent partial prefill,
versus entering a multi-minute engine restart/load cycle. If the candidate is
a different image, run the same import with `docker run --rm --entrypoint
python <engine-image> -c ...` before assigning GPUs. Record preflight wall time
and the result in `EXPERIMENTS.md`.

## Benchmarks (run on node06)

All benches are in `bench/` and mirrored to
`/home/luke/inference/dspark_0731/`. Export the token first:

```bash
ssh node06
export BENCH_TOKEN=$(grep -o 'Bearer [A-Za-z0-9_-]*' /etc/caddy/Caddyfile | head -1 | cut -d' ' -f2)
```

## Fast iteration rules

- Keep exactly one Rust lane against the shared `target/`: do not overlap
  Clippy, tests, or release builds. Run Python protocol/benchmark tests and
  Compose validation beside that one Rust lane because they do not consume its
  artifacts. Keep Cargo's shared target and Docker BuildKit cache warm; do not
  clean either between runs.
- Drone runs one pipeline for each PR revision and once after merge on `main`,
  not again for the corresponding feature-branch push. The quality pipeline's
  concurrency limit serializes superseded revisions: #260/#261 proved that two
  simultaneous cold Rust lanes can starve deadline-sensitive transport tests.
  Cancel a superseded running build when safe instead of letting the queue grow.
  Its Rust lane omits a redundant release
  build: the local pre-push gate builds release, and Drone's post-merge main
  pipeline builds and publishes the release container to GHCR. The
  `publish-image` step reads `ghcr_username`/`ghcr_token` only on a main push;
  pull-request containers never receive those secrets. GitHub Actions is not a
  second CI system for this repository. Drone's PR quality budget is currently
  about 58s; inspect and record regressions instead of adding duplicate gates.
  The release Dockerfiles inherit the content-keyed `rust-deps-sha256-*` image
  and compile the repository crate with Cargo offline. This is an explicit
  build input supported by the runner's Docker 20.10 daemon, not inline-cache
  metadata. The dependency guard is shared by the LB and companion; both app
  publishers start only after it reports success. On a nonmatching push, each
  step exits successfully before Docker startup or registry login. Do not rely
  on `when.paths`: build #245 proved this Drone version discards that field and
  consequently ran all three publishers for a deployment/CI-only diff. Treat
  any Docker activity on an equivalent diff as a guard regression. Source-only
  builds should pay the base pull plus one crate relink, never dependency
  compilation. Build #233's 135s/129s publishers remain the dependency-cache
  regression control. #256/#258 proved guarded `plugins/docker` steps lose
  automatic plugin privilege, while this Drone repo is untrusted and cannot
  request explicit privilege. Main publishing therefore uses a digest-pinned,
  unprivileged Kaniko executor after the same shell guard, with a GHCR remote
  layer cache. Keep credentials scoped to publisher steps and never add
  privilege to PR, main, or tag-copy steps.
  Node06 deploys should keep using the warm local `bench/build_transfer.sh`
  path because it reuses the development BuildKit cache.
- Release tags use the separate `release-tags` Drone pipeline. It triggers only
  for `refs/tags/v*`, repeats the full Rust/Python/Compose quality gate, and
  promotes only the already-qualified `rust-$shortsha` and
  `companion-rust-$shortsha` manifests to `v<crate-version>` and
  `companion-v<crate-version>`. It never rebuilds or publishes edge aliases.
  `bench/drone_release_plan.sh` requires the tag, ref, checked
  out commit, and Cargo package version to agree before producing private
  revision-bound markers; each publisher rechecks its marker and tag event
  without Git. The registry publisher verifies exact source/version/revision
  OCI labels, copies with `crane`, and requires destination digest equality.
  An already-present destination is accepted only when its digest is identical;
  a different digest is an immutable-tag conflict, and an ambiguous lookup
  failure is never treated as absence.
  Cargo build metadata (`+...`) is rejected because it is not a valid Docker
  tag. Merge, build, deploy, and qualify the SHA-tagged images before creating
  the release tag. Confirm the qualified refs are
  `rust-$shortsha@sha256:<digest>` and
  `companion-rust-$shortsha@sha256:<digest>`, then create the exact
  `v<crate-version>` tag. The pipeline must report the same source and
  destination digest for each manifest. Never create a tag merely to test the
  pipeline; unit/static tests and Drone PR quality are
  the non-publishing acceptance path.
- Crane's published image has `/busybox/sh` but no `/bin/sh`, and Drone 2.12
  silently discarded an attempted step-level entrypoint override. Failed tag
  build #266 and recovery #269 therefore exited both publishers before commands;
  neither performed a registry operation. Release publishers run instead in the
  content-keyed `Dockerfile.release-tools` image: digest-pinned Alpine supplies
  `/bin/sh`, and the Crane binary is copied from its digest-pinned upstream image
  and smoke-tested during the build. `bench/release_tools_image.py` binds all
  consumers and the sole guarded Kaniko publisher to the Dockerfile hash. Never
  point release steps directly at an unknown third-party shell layout. After
  main publishes a new tools content tag, independently verify `/bin/sh`, Crane,
  and CA roots, then pin every tag/recovery consumer as `tag@sha256:digest`; the
  publisher destination itself remains tag-only. A digest-pin-only follow-up
  must select zero image publishers.
  The narrowly scoped
  `release-v0.1.0-recovery` promote pipeline is the only recovery: it accepts
  target `release-v0.1.0`, fetches and peels the existing immutable tag to
  `b0e070073d4266018d2f907ff35a7ee88adfdcd4`, checks that tag's Cargo version,
  and copies only the two qualified `b0e0700` manifests. Do not generalize it,
  move the tag, overwrite a destination, or run it after both destinations are
  verified; remove the recovery document after the v0.1.0 release is complete.
- Build the LB locally and stream it to node06 when the local amd64 Docker
  cache is warm. A typical warm LB transfer is seconds and avoids consuming
  node06's scarce 8-9GiB available host memory:

  ```bash
  docker build -t ghcr.io/helixml/mini-dynamo:<tag> .
  docker save ghcr.io/helixml/mini-dynamo:<tag> | gzip -1 | \
    ssh node06 'gunzip | docker load'
  ```

- Recreate only `ds4-loadbalancer` for router work. The engines retain their
  KV caches and the interruption is about four seconds. Trigger one small
  allocation on each engine after the roll so late-subscriber replay starts;
  wait for both exact inventories to become trusted before an exact test.
- Do not trigger another full-history replay on long-lived A merely to profile
  the receiver. r33 already measured a 177.52s publisher gap while Rust spent
  only 2.12s decoding and 0.16s folding 5,500 batches. Use the exported replay
  phase/progress metrics, mock captured-shape tests, or the snapshot companion;
  a production no-op-fold replay adds publisher pressure without new evidence.
- Run the issue #41 captured-shape gate locally; it is GPU-free and should take
  well under a second once the release example is built:

  ```bash
  cargo run --release --locked --example snapshot_shape_bench
  cargo run --release --locked --example digest_index_prototype
  ```

  Keep compiler RSS out of runtime measurements by timing the built binaries
  directly under `target/release/examples/`. The 2026-08-13 foundation measured
  10.8ms validated snapshot decode and 27MiB standalone peak RSS at 36,612
  records; a large regression should fail before any node06 rollout.
- For an engine candidate, stop at the first failed gate: source/fixture checks,
  five-case correctness smoke, c8 scout, then the full matrix. A cached valid
  candidate should reach the first decision in roughly 15-18 minutes, dominated
  by engine startup.
- Once a candidate passes correctness, run matched direct matrices on A and B
  concurrently so two TP4 pairs finish in the slower engine's time. Production
  must remain single-homed; discard a control interval if live traffic reaches
  its engine-native counters. Pay for a two-round GPU crossover only when the
  scout is close enough to a promotion threshold to justify both warm starts.
- Long scripts must emit bounded progress and support resume. Capture identity,
  restart count, JIT markers, and client/native token reconciliation per cell so
  a failed late gate does not invalidate earlier clean cells.

### Cache locality — `locality_bench.sh BASE APPS SESSIONS TURNS`

Simulates APPS apps (each a distinct ~18.5k-token system prompt) × SESSIONS
sessions × TURNS turns, sequential. Reports per-request `prompt cached wall`
and the total cache-hit %. **Use a fresh `SALT`** each run so prompts are
unseen (otherwise prior runs left them warm on both engines and you measure
nothing):

```bash
SALT=$(date +%s) ./locality_bench.sh http://127.0.0.1:8006 3 4 2
# cold prefills = rows with cached==0; count them:
#   awk 'NF==6 && $5==0' <output>
```

### Cache scorecard — `cachebench.py BASE MODEL`

Prefer the Python scorecard when changing cache metrics or measuring a working
set. It streams synthetic requests, round-robins apps before reuse, and
requires response usage, LB counters, and summed native engine counters to
reconcile. A zero-spread run is evidence that unrelated production traffic did
not contaminate the cell:

```bash
python3 cachebench.py http://127.0.0.1:8006 deepseek-v4-flash \
  --apps 1,4,8 --sessions 2 --turns 2 --prefix-kib 32 \
  --metrics-url http://127.0.0.1:8007/metrics \
  --engine-metrics http://127.0.0.1:8012/metrics \
  --engine-metrics http://127.0.0.1:8013/metrics \
  --salt "$(date +%s%N)" --require-reconciled
```

Always use a fresh salt. With one app and one session, 2/4/10/20/100 turns
produce controlled request-reuse targets of 50/75/90/95/99%; do not call that
the token hit ratio—the runner reports both separately. Increase app count and
prefix size to grow the working set. Keep these cells sequential because
reuse distance, cache residency, and counter deltas are the experiment.

### Concurrent same-app load — `concurrent_sameapp.sh BASE N SALT TOK`

The test that separates the routers: N concurrent sessions sharing one system
prompt. Prints the upstream A/B split + aggregate tok/s. A load-blind router
sends all N to one instance; a good one spreads them.

```bash
./concurrent_sameapp.sh http://127.0.0.1:8006 12 $(date +%s) 256
```

### Aggregate throughput regression — `bench_serving.sh N TOK`

Standard N-way mixed sweep. Run after any change to confirm no throughput
regression. The current rc7 box code gate is in the 1,820–1,844 tok/s class at
c24/max256; the box is shared with live traffic, so expect run-to-run noise.

### Agent protocol regression — `agentbench.py` / `agent_matrix.sh`

Run the committed synthetic corpus locally before using GPUs. It catches split
DSML marker leakage, malformed or wrongly typed tool arguments, parallel-call
assembly, and missing reasoning/tool history:

```bash
python3 bench/agentbench.py validate
python3 -m unittest discover -s bench -p 'test_*.py'
```

On node06, generate provenance and run a focused smoke first. The runner never
prints completion content, reasoning, or arguments:

```bash
bench/node06_agent_metadata.sh /tmp/agent-metadata.json
python3 bench/agentbench.py run http://127.0.0.1:8006 deepseek-v4-flash \
  --metadata-json /tmp/agent-metadata.json --profile deterministic \
  --concurrency 1 --repetitions 1
```

Use `agent_matrix.sh BASE MODEL LABEL` for the qualification matrix. Defaults
cover deterministic and official agentic sampling, 0/256KiB shared prefixes,
c1/c8/c16, and cold/warm passes. Narrow `AGENT_PROFILES`,
`AGENT_PREFIX_KIBS`, or `AGENT_CONCURRENCIES` in the development loop; set
`AGENT_RUNS=3` only for a final variance-qualified candidate. Run direct-engine
A/B cells with the two-round crossover below to use both TP4 pairs at once.

### Long-prefill interference — `mixed_bench.py`

For direct-engine scheduler trials, point `METRICS_URL` at the same engine.
The runner then snapshots mean engine queue/prefill time and preemptions, while
a 20ms sampler records peak running/waiting/KV usage:

```bash
METRICS_URL=http://127.0.0.1:8013/metrics \
  MIXED_ORDER=prefill-first SALT=$(date +%s) \
  python3 bench/mixed_bench.py http://127.0.0.1:8013 \
    deepseek-v4-flash 52000 8 256 3
```

Single-home production on the other engine before interpreting metric deltas;
otherwise unrelated traffic contaminates engine-global counters. Run both
prefill-first and decode-first orders with fresh salts. Queue/prefill means
come from engine histogram deltas; request TTFT p95 remains the latency gate.

### Direct engine matrix — `engine_matrix.sh BASE MODEL LABEL`

For a rolling engine/image A/B, keep production single-homed on the other
engine and run the matched code+prose c1/c8/c16 matrix directly. The output is
six JSONL records with usage-token throughput, TTFT, and DSpark acceptance.
Point `METRICS_URL` at the same engine so production-wide counters do not
contaminate the speculative deltas:

```bash
METRICS_URL=http://127.0.0.1:8013/metrics \
  ./engine_matrix.sh http://127.0.0.1:8013 deepseek-v4-flash candidate \
  | tee /tmp/candidate-matrix.jsonl
```

Do not interpret draft acceptance alone when comparing capacity policies: a
pruned policy can raise the percentage by reducing its denominator. Compare
effective tokens/step and end-to-end throughput as well.

### Fail-fast engine qualification — `candidate_gate.py`

Every new engine image must pass the five-request deterministic agent gate
before a performance scout or full matrix. Capture candidate and direct-engine
metadata once, then keep the same files for resume:

```bash
bench/node06_engine_metadata.sh /tmp/candidate-engine.json dspark-0731-b \
  /tmp/upstream-receipt.json
BENCH_GPU_COUNT=4 bench/node06_agent_metadata.sh \
  /tmp/candidate-agent.json dspark-0731-b

python3 bench/candidate_gate.py \
  --base http://127.0.0.1:8013 --model deepseek-v4-flash \
  --container dspark-0731-b \
  --engine-metadata /tmp/candidate-engine.json \
  --agent-metadata /tmp/candidate-agent.json \
  --output /tmp/candidate-gate.jsonl --through smoke
```

Only a green smoke may continue to `--through scout --resume` (one code and
one prose c8 cell), then `--through matrix --resume`. Resume requires the same
immutable candidate, process lifetime, metadata, and hashed plan/scripts.
Every boundary rechecks image/start/restart identity and scans only that
stage's container-log interval for late JIT, CUDA, NCCL, OOM, Xid, traceback,
or fatal-runtime markers. A correctness/runtime failure stops before the next
GPU stage. The mode-0600 journal contains only identity/plan hashes, bounded
result classes, artifact hashes/sizes, and timing; privacy-safe child JSONL
lives in a mode-0700 artifact directory. Container logs, commands, environment
variables, prompts, responses, and credentials are not written.

`engine_matrix.sh` retains its six-cell default. Narrow a scout with
`ENGINE_WORKLOADS`, `ENGINE_CONCURRENCIES`, and `ENGINE_RUNS`; each cell and
the full matrix emit wall-clock timing records.

Before an engine image A/B, capture each engine separately with
`bench/node06_engine_metadata.sh OUTPUT CONTAINER [RECEIPT]`. A supplied receipt
must verify before its benchmark cells are admissible. Never combine two
engines into one provenance string: their image, lifetime, runtime packages,
effective argv, JIT interval, and metrics must remain independently
attributable. Run matched direct cells on A and B concurrently only after both
are warm; keep cache/LB capacity cells serial because cross-traffic
contaminates their residency result.

When single-homing the LB, reduce `DS4_KV_EVENT_LIVE_ENDPOINTS` and
`DS4_KV_EVENT_REPLAY_ENDPOINTS` to the same cardinality as `DS4_UPSTREAM` in
the same recreate. Render candidate engine argv with `DRY_RUN=1` before a GPU
roll, persist a runtime-versioned JIT cache, and discard every performance
interval containing an inference-time JIT marker. Correctness gates precede
parameter sweeps: a deterministic agent-protocol failure ends that image's
experiment before paying another engine warm-start cost.

### A/B protocol

To compare two LB images cleanly: deploy image X, run a bench with a fresh
salt, capture; deploy image Y, run the SAME bench with ANOTHER fresh salt,
capture; compare. Never reuse a salt across the two — warm state leaks.
Read the router's own decisions with
`curl -s :8007/metrics | grep ds4proxy_route_decisions_total`.


### Using both TP4 pairs without invalidating the result

Parallelize direct-engine work when the two jobs are independent. For a
baseline/candidate engine comparison, use a two-round crossover with fresh
inputs in every cell:

| round | engine A (`:8012`) | engine B (`:8013`) |
|---|---|---|
| 1 | baseline | candidate |
| 2 | candidate | baseline |

Launch the two cells in a round together and point each collector at that
engine's metrics endpoint. This removes much of the time drift and engine bias
while using all eight GPUs. It is appropriate for code/prose/agent matrices,
context sweeps, tokenizer/protocol checks, and one-variable engine settings.

Keep these tests serial because concurrent runs change the state being
measured or contend on the same replicas:

- cache locality, eviction, and cold/warm comparisons;
- LB policies that choose between both engines;
- aggregate box-capacity gates;
- exact-placement tests whose answer depends on both inventories.

Use offline route replay, unit tests, mock upstreams, and direct-engine probes
to eliminate candidates before a serial GPU A/B. Do not run two nominally
parallel jobs merely to make the clock shorter if their GPU load, KV warming,
or event streams contaminate one another.


### Decision journal + offline replay (rc5+)

Answer "would a different alpha/cap have routed better?" without touching
production: enable the journal, capture real traffic, replay counterfactuals.

```bash
# enable (LB env; stdout JSONL, privacy-bounded: no prompts/fingerprints/hosts)
ssh node06 'cd /home/luke/inference/dspark_0731 &&   DS4_ROUTE_JOURNAL=true LB_IMAGE=<current tag> docker compose up -d ds4-loadbalancer'

# capture + replay a policy sweep locally
ssh node06 'docker logs ds4-loadbalancer 2>&1' > /tmp/trace.log
python3 bench/route_replay.py /tmp/trace.log --alphas 1,2,4,8 --caps 8,16,32,64
```

Replay holds each observed cache/load snapshot fixed (the journal keeps no
prefix identity), so it compares single-decision policies — it cannot
simulate cache drift caused by earlier counterfactual choices. Findings and
caveats: EXPERIMENTS.md "rc5 privacy-bounded decision journal and replay".

## Full experiment checklist

The repeatable end-to-end loop (each past run is written up in
EXPERIMENTS.md — add yours there too):

1. **Local**: run the Rust gate plus `python3 bench/agentbench.py validate`
   and `python3 -m unittest discover -s bench -p 'test_*.py'`; add/extend a
   router test for any routing change.
2. **Build + deploy candidate** on node06 (section above) with a fresh
   `<tag>`; confirm `ds4proxy_upstream_up` shows both engines and the boot
   log line has the config you meant to ship.
3. **Correctness before capacity**: for an engine candidate, run
   `candidate_gate.py --through smoke`; continue through its c8 scout and full
   direct matrix only while every prior stage is green and free of runtime/JIT
   markers.
4. **Bench matrix** (fresh SALT per run, per the A/B protocol):
   locality (`locality_bench.sh`), concurrent same-app
   (`concurrent_sameapp.sh`), aggregate regression (`bench_serving.sh 16 512`),
   and the focused agent protocol smoke. Run the full agent matrix only for
   engine/parser/router candidates that can affect the protocol or headline
   performance.
5. **Route telemetry**: `curl -s :8007/metrics | grep -E
   "route_decisions|route_overlap|upstream_inflight|upstream_load_units"` —
   confirm the decision mix moved the way the change predicts.
6. **Helix end-to-end**: one real session via `POST $HELIX_URL/api/v1/
   sessions/chat` against the org test app (ids + creds: infra repo,
   `node06/inference/dspark_0731/README.md`). This catches harness-shim
   regressions that synthetic benches miss.
7. **Record**: append the run to EXPERIMENTS.md (config, numbers, verdict),
   update RESULTS.md if it changes a headline, and either promote the tag in
   the canonical mini-dynamo Compose (`LB_IMAGE` default) or note why not.
8. **Mirror a promoted config**: validate the canonical Compose file, run
   `deploy/dspark_0731/sync-compose.sh ../infra`, and commit the infra
   mirror. Never hand-edit the infra copy.
9. **Watch after promote**: Grafana `ds4-flash-serving` for 10-15 min
   (5xx, TTFT p95, upstream split) — rollback is one `LB_IMAGE` flip.

## Guardrails

- The LB sits in the **production inference path** (Helix agent fleet). Bench
  traffic shares the box with real users — keep concurrency modest and
  prefer short `max_tokens`. Never stop the engines to test the LB.
- Secrets (Caddy bearer, Helix API key) are fetched on-box, never committed.
- Metric names keep the `ds4proxy_` prefix for dashboard continuity — do not
  rename without updating `clusters/bunker/monitoring/` in the infra repo.
- Verify end-to-end through Helix after a deploy, not just the LB:
  a chat via `POST $HELIX_URL/api/v1/sessions/chat` with the org's app id
  (connection details in the infra repo's `node06/inference/dspark_0731/
  README.md`, not here).
