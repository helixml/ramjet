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
