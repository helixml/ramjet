# node06 DeepSeek V4 Flash stack

This directory is the canonical source for node06's complete Docker Compose
stack: the Rust mini-dynamo load balancer and both TP4 vLLM/DSpark engines.
The deploy checkout on node06 and the infra repository contain mirrors so
operators can still use the established `/home/luke/inference/dspark_0731`
working directory.

Edit `docker-compose.yaml` here first. Keep secrets in node06's uncommitted
`.env`; never add them to either repository.

## Fast validation and mirroring

```bash
docker compose -f deploy/node06/dspark_0731/docker-compose.yaml config --quiet
deploy/node06/dspark_0731/sync-compose.sh --check ../infra
deploy/node06/dspark_0731/sync-compose.sh ../infra
```

`--check` is read-only and exits nonzero if the infra mirror differs. The
sync command updates only the mirrored Compose file; it does not touch the
operational README, benchmark helper, `.env`, containers, or engines.

After the mini-dynamo and infra changes are merged, update node06 explicitly:

```bash
scp deploy/node06/dspark_0731/docker-compose.yaml \
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
  deploy/node06/dspark_0731/validate-snapshot-companion-host.sh

python3 deploy/node06/dspark_0731/validate-snapshot-companion-compose.py
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
wiring must set exactly `DS4_SNAPSHOT_METRICS_SOCKET_PATH` and
`DS4_SNAPSHOT_METRICS_GROUP_GID` (not `DS4_SNAPSHOT_METRICS_BIND`), mount a
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

DS4_SNAPSHOT_ENGINE_METADATA_PATH=/run/mini-dynamo-snapshot-a/engine-metadata.json \
DS4_SNAPSHOT_DIGEST_SECRET_PATH=/run/secrets/mini-dynamo-snapshot-digest-a \
DS4_SNAPSHOT_ATTESTATION_PATH=/run/secrets/mini-dynamo-snapshot-attestation-a \
DS4_SNAPSHOT_SECRET_OWNER_UID=0 \
DS4_SNAPSHOT_SECRET_GROUP_GID=12000 \
  /usr/local/libexec/mini-dynamo-attestation-provisioner
```

The default capture freshness limit is 30 seconds and can be reduced or raised
to at most five minutes with `DS4_SNAPSHOT_ATTESTATION_MAX_AGE_MS`. The metadata
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

This slice remains offline. Do not enable either companion yet: production
per-engine service-manager/Compose wiring and the dual-domain validation gate
remain separate rollout requirements.
