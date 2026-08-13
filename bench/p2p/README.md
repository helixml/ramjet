# Node06 direct-P2P prerequisite gate

This tooling addresses issue #32 Phase B without changing driver settings. It
separates CUDA-kernel peer access from copy-engine DMA and NCCL:

- NVIDIA `nvbandwidth` `device_to_device_memcpy_*_sm` executes SM copy kernels;
- the corresponding `*_ce` cases use `cuMemcpyAsync`;
- NVIDIA `nccl-tests` provides a small-message four-rank AllReduce control.

Sources and runtime are immutable:

- `NVIDIA/nvbandwidth` v0.10.0 commit
  `82fc4e8c6afa0babb8687793678f615b3b8d793e`;
- `NVIDIA/nccl-tests` commit
  `717b68318278e93f371d8ffb46b076069d7c7851`;
- r34 runtime
  `voipmonitor/vllm@sha256:820181fbbc975cd5291c411cda9771d58fecee1636d916f508f47230df20592b`.

## Build off node06

Build on the development machine. The script fetches only the exact commits,
compiles inside the exact r34 image, exports two binaries plus a SHA-256
manifest, and never contacts node06:

```bash
mkdir -p /home/karolis/.cache/mini-dynamo-p2p-tools
bench/p2p/build_tools.sh /home/karolis/.cache/mini-dynamo-p2p-tools
scp -r /home/karolis/.cache/mini-dynamo-p2p-tools node06:/tmp/
```

Record the manifest digest from the development host through a separate trusted
channel, then make the transferred tree root-owned and immutable. Do not copy a
digest from node06's adjacent manifest and treat it as external evidence:

```bash
sha256sum /home/karolis/.cache/mini-dynamo-p2p-tools/manifest.json
ssh node06 'chown -R root:root /tmp/mini-dynamo-p2p-tools && \
  chmod 0555 /tmp/mini-dynamo-p2p-tools \
    /tmp/mini-dynamo-p2p-tools/nvbandwidth \
    /tmp/mini-dynamo-p2p-tools/all_reduce_perf && \
  chmod 0444 /tmp/mini-dynamo-p2p-tools/manifest.json'
```

Do not compile in a live engine container. Active mode accepts only a
root-owned, non-writable, non-symlink tree matching the external manifest hash,
then copies verified file descriptors into a private root-owned staging tree.

## Read-only default

On node06, the default performs only source-independent host/container
preflight. It discovers the target GPU reservation from Docker, maps it to
current NVIDIA UUIDs, verifies those UUIDs are owned only by the target engine,
and requires exact r34 image, restart-zero engines, NUMA pinning, NODE topology,
and peer read/write capability. It prints no environment or credential:

```bash
python3 bench/p2p/node06_phase_b.py
python3 bench/p2p/node06_phase_b.py --print-plan
```

## Explicit active modes

Active mode is not an ordinary development command. It briefly recreates only
the LB to single-home production on engine A, proves engine B counters are
unchanged for at least 60 seconds, then starts a fresh, networkless, read-only,
capability-free tool container on the UUID-validated B reservation. Engines
are never restarted.

The minimal 1MiB/two-GPU scout requires both switches:

```bash
sudo python3 bench/p2p/node06_phase_b.py \
  --run-gpu-scout \
  --expected-tools-manifest-sha256 <trusted-development-host-sha256> \
  --acknowledge-production-risk I_ACKNOWLEDGE_NODE06_PRODUCTION_RISK
```

The complete 64MiB TP4 SM/CE/latency matrix plus NCCL control is separately
explicit. Use three cycles only for the qualified before/after comparison:

```bash
sudo python3 bench/p2p/node06_phase_b.py \
  --run-full-prerequisite --cycles 3 \
  --expected-tools-manifest-sha256 <trusted-development-host-sha256> \
  --acknowledge-production-risk I_ACKNOWLEDGE_NODE06_PRODUCTION_RISK
```

Every tool container has `network=none`, private IPC, read-only root, all
capabilities dropped, no-new-privileges, fixed CPU/NUMA/memory/PID limits, an
ephemeral `/tmp`, only the read-only tool mount, and a 60/180/120-second
scout/matrix/NCCL timeout. The harness polls control health during each run,
creates each tool container first, retains its exact ID, attaches separately,
and unconditionally removes that ID on timeout, health loss, client failure, or
signal. Signals are deferred while the immutable captured Compose render is
restored and are propagated only after 2/2 health and the complete runtime spec
match. The harness hashes Compose and `.env` before mutation, rejects source
drift, captures the complete rendered baseline plus service config hash, and
refuses active mode if the running LB has missing or unexpected environment or
other relevant spec differences.

Results live under a newly created mode-0700
`/tmp/mini-dynamo-p2p-phase-b.*` directory; each file is mode 0600. Treat the
entire directory as sensitive: the immutable Compose baseline intentionally
contains the full rendered LB environment so restoration does not depend on a
mutable `.env`. Other artifacts contain topology/tool identities, counters, and
benchmark output, never prompts or responses. Do not copy, commit, or weaken
permissions on the result directory.

## Interpretation and runtime

CE and NCCL healthy while SM read/write is dramatically slower, asymmetric, or
unstable supports the missing direct-SM peer-path hypothesis. Both SM and CE
slow points to a common topology/ACS/IOMMU limitation. SM near CE weakens the
hypothesis. No one absolute GB/s number authorizes a driver change: retain the
same artifact and commands for before/after, require a reproducible SM-only
improvement while CE/NCCL stay within noise, then run the real custom-AllReduce
crossover in a maintenance window.

The scout takes roughly 2-3 minutes including the mandatory quiet fence and LB
restore. One full cycle takes roughly 6-8 minutes; three cycles may take 10-15
minutes. Single-homing halves redundancy and capacity, and a second CUDA context
uses some of B's limited memory. This is low-impact, not zero-impact. If zero
service effect is required, use a separate host or maintenance window.

The first off-box build can take 10-20 minutes because the exact r34 base image
and CUDA build dependencies are cold; an unchanged repeat should use BuildKit's
cache. Copying the small exported artifact normally takes seconds. Neither
build nor transfer is part of the active runtime estimate above.
