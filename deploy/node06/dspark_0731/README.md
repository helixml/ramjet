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

