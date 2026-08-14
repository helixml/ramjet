# Boot-time /run authority for the snapshot companion

Admission artifacts for issues #156/#157/#158. **Nothing here is installed on
node06 yet**, and installing it is not a prerequisite for serving — it is a
prerequisite for ever applying `docker-compose.snapshot-lb.yaml`.

## Why

`/run` is tmpfs. Every socket parent, attestation file, and session/digest
secret the companion owns is destroyed on reboot, and nothing recreated them.
After the 2026-08-14 12:10 UTC reboot the LB could not be *created* — runc
failed the mount request for `/run/mini-dynamo-snapshot-attestation-a`, exit
127. A create-time failure is not a process exit, so `restart: unless-stopped`
never retried and `RestartCount` stayed `0`. Production served nothing for
~50 minutes and nothing alerted (#159).

## What

| File | Role |
| --- | --- |
| `tmpfiles.d/mini-dynamo-snapshot.conf` | Directory parents only, with the companion's own ownership/setgid/mode policy. Applied by `systemd-tmpfiles-setup.service`, long before `docker.service`. |
| `mini-dynamo-snapshot-authority.service` | `oneshot` running `setup_snapshot_production_host.py`, then its read-only `--check`. Ordered `Before=docker.service`. |

The split is deliberate: a tmpfiles fragment must never invent secret material,
and the provisioner must never be the thing standing between a reboot and a
directory existing.

## Failure semantics

The unit is pulled in by `WantedBy=docker.service`, **not** `RequiredBy=`.
Ordering is guaranteed; failure is not propagated. If provisioning fails:

- `docker.service` still starts.
- `ds4-loadbalancer` still starts and serves — it has no `/run` mounts unless
  `docker-compose.snapshot-lb.yaml` was explicitly applied.
- The companions stay down and `systemctl --failed` shows the unit.

#158 asks for `RequiredBy=docker.service`, which would make a provisioner
failure block Docker and therefore block serving. That contradicts the same
issue's third acceptance bullet ("must not block the LB from serving"), so the
safety property wins and the ordering is expressed with `Wants=`.

## Install

```bash
install -D -m 0755 deploy/dspark_0731/setup_snapshot_production_host.py \
  /usr/local/lib/mini-dynamo/setup_snapshot_production_host.py
install -D -m 0644 deploy/dspark_0731/systemd/tmpfiles.d/mini-dynamo-snapshot.conf \
  /etc/tmpfiles.d/mini-dynamo-snapshot.conf
install -D -m 0644 deploy/dspark_0731/systemd/mini-dynamo-snapshot-authority.service \
  /etc/systemd/system/mini-dynamo-snapshot-authority.service

systemd-tmpfiles --create /etc/tmpfiles.d/mini-dynamo-snapshot.conf
systemctl daemon-reload
systemctl enable --now mini-dynamo-snapshot-authority.service
```

## Verify the acceptance condition

Simulated `/run` teardown must recover without manual steps:

```bash
systemctl stop docker
rm -rf /run/mini-dynamo-snapshot-* /run/secrets/mini-dynamo-snapshot-*
systemctl restart systemd-tmpfiles-setup.service
systemctl restart docker            # pulls in the authority unit first
setup_snapshot_production_host.py --check
```

And provisioner failure must not take serving with it:

```bash
systemctl stop docker
# make the provisioner fail, e.g. by pre-creating an unsafe secret path
systemctl restart docker
docker ps --filter name=ds4-loadbalancer   # must still be Up
systemctl --failed                          # must list the authority unit
```
