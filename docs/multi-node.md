# Routing across many nodes

One ramjet can front engines on any number of machines. It routes whole
requests over HTTP, so a node only has to expose its engine ports on a network
the balancer can reach. Nodes need no GPU interconnect between them. A replica
that itself spans nodes (tensor parallelism over InfiniBand) is the engine's
concern; ramjet still sees one URL for it.

What changes with fleet size is what the affinity score sees. On a
two-replica box every replica soon holds every application's shared prompt.
Across twenty nodes most replicas hold a few applications, and a session's
history lives on exactly one. The settings below are the ones that keep
sessions on the replica holding their history as the fleet grows.

## Describe the fleet in a topology file

At ten or twenty nodes, `RJ_UPSTREAM` and its parallel lists are forty entries
long and have to stay aligned by hand. `RJ_TOPOLOGY_FILE` names the same fleet
as nodes and replicas:

```json
{"nodes": [
  {"name": "h200-01", "replicas": [
    {"url": "http://10.0.0.11:8070", "model": "glm-5.3", "kv_capacity_tokens": 2050000},
    {"url": "http://10.0.0.11:8071", "model": "glm-5.3", "kv_capacity_tokens": 2050000}
  ]},
  {"name": "h200-02", "replicas": [
    {"url": "http://10.0.0.12:8070", "model": "glm-5.3", "kv_capacity_tokens": 2050000},
    {"url": "http://10.0.0.12:8071", "model": "glm-5.3", "kv_capacity_tokens": 2050000}
  ]}
]}
```

Each replica takes `url` and optionally `model`, `api` (`openai` or
`systemone`), `speculation_profile`, `kv_capacity_tokens`, and `dp_ranks` (see
below). The file expands into `RJ_UPSTREAM`, `RJ_UPSTREAM_MODELS`,
`RJ_UPSTREAM_APIS`, `RJ_UPSTREAM_DP_RANKS`, `RJ_ROUTE_SPECULATION_PROFILES`,
and `RJ_ROUTE_KV_CAPACITY_TOKENS`, so every
existing per-upstream check applies and upstream ordinals follow file order.
Setting one of those variables as well is a startup error, not a merge.
`model` and `speculation_profile` go on every replica or none. Unknown fields,
duplicate node names or URLs, and empty nodes are rejected.

Node names appear as the `node` label of `ramjet_upstream_info` and in each
`/health` replica entry. Without a topology file, `ramjet_upstream_info` uses
the URL host and `/health` omits the field, because `/health` never publishes
upstream addresses.

## Data-parallel attention ranks

SGLang's DP attention gives each GPU rank its own KV cache. That multiplies
a node's cache capacity, but only if every turn of a session reaches the rank
holding it. SGLang's built-in balancers (round robin, request counts, token
counts) ignore prefixes. Give the replica `"dp_ranks": 8` and it expands into
eight upstreams that share the URL. Each one pins its requests with
`routed_dp_rank`, and prefix affinity chooses the rank:

```json
{"nodes": [
  {"name": "h200-01", "replicas": [
    {"url": "http://10.0.0.11:8073", "model": "glm-5.3", "dp_ranks": 8}
  ]}
]}
```

On one 8×H200 node serving GLM-5.3 FP8 (DP8 attention, DeepEP, FP8 KV,
193k tokens per rank), in a coding-agent swarm with 16 developers:

| rank placement | turns/min | prompt cached | TTFT p50 / p90 |
|---|---:|---:|---:|
| SGLang round robin | 43.0 | 65.6% | 2.6s / 5.0s |
| ramjet, one upstream per rank | 71.9 | 92.9% | 0.75s / 1.7s |

With 64 developers the working set exceeds the device pools. The pinned fleet
still led: 46.9 against 42.0 turns/min, and 72.4% against 56.6% cached.

`routed_dp_rank` is appended as the last key of the JSON body. The engine
keeps the last duplicate key, so a caller cannot choose a rank the router did
not.

A rank can wedge on its own. On H200 we saw one DP rank's scheduler stop after
a contained GPU error while `/health` and `/v1/models` kept answering 200.
`RJ_UPSTREAM_RANK_PROBE=on` adds a one-token generation pinned to each rank to
its readiness probe. A timeout fences only that rank's upstream, unless the
rank completed real traffic in the last 30 seconds, since a busy rank queues
the probe behind real work.

## Recommended settings

```yaml
RJ_TOPOLOGY_FILE: /etc/ramjet/topology.json
RJ_ROUTE_AFFINITY_BASIS: relative
RJ_ROUTE_MAX_ATTEMPTS: "3"
RJ_UPSTREAM_CONNECT_TIMEOUT_MS: "2000"
RJ_UPSTREAM_RANK_PROBE: "on"      # with dp_ranks
RJ_UPSTREAM_TOKEN: ${ENGINE_BEARER}
```

- **`relative` affinity.** `marginal` is exact for two replicas but uses the
  least-warm peer as its floor, and in a larger fleet that peer is usually
  cold, so it degrades to `absolute`. `relative` measures from the warmest
  peer instead and makes the same decisions as `marginal` at two replicas. See
  the table below.
- **Bounded, node-aware failover.** A request otherwise tries every serving
  replica in turn. When a node disappears, its replicas stay routable until
  the next probe, and each attempt can cost a full connect timeout. After a
  failure, ramjet tries replicas on other nodes before the failed replica's
  siblings. A refused connection also marks the engine's other DP ranks down
  immediately.
- **Short connect timeout.** The 30s default suits a Docker network on one
  host. It does not suit a machine that has dropped off the network.
- **One bearer for all engines.** `RJ_UPSTREAM_TOKEN` is shared by every
  upstream, so keep the engine ports on a private network or VPN.

Health probes for healthy replicas run concurrently (at most eight at a time)
under `http` admission, so a probe round stays within the 15s interval at 40
replicas. Compatibility admission still probes healthy replicas one at a time,
because it must never fence the last admitted replica.

## What the simulation shows

`tests/fleet_routing_simulation.rs` drives the real router with a coding-agent
fleet. It has twelve applications whose shared prompts exceed the affinity
cap, six developers per replica whose context grows every turn, H200-sized
per-replica caches, and overlapping request lifetimes on a virtual clock.
Stickiness is the share of follow-up turns sent to the replica that served the
session's previous turn. Recompute is the share of prompt blocks the chosen
replica did not hold.

| replicas | absolute | marginal | relative | recompute, absolute → relative |
|---:|---:|---:|---:|---:|
| 2 | 93.7% | 99.1% | 99.1% | 7.0% → 6.4% |
| 4 | 94.6% | 94.6% | 98.9% | 6.7% → 6.2% |
| 10 | 89.5% | 89.5% | 97.6% | 7.0% → 6.1% |
| 20 | 68.6% | 68.6% | 93.1% | 9.5% → 6.3% |
| 40 | 51.2% | 51.2% | 90.0% | 12.2% → 6.4% |

Load stays spread: the busiest replica served 1.5–1.8× the mean under every
basis. The recompute column is an estimate of the prefill each placement costs,
because the router's own index stands in for the engines' caches.
Stickiness is an exact property of the decisions. Reproduce the table with:

```bash
cargo test --release --test fleet_routing_simulation -- --ignored --nocapture
```

## Routing cost

Scoring walks every replica's served-prefix index under one lock, so it grows
with replicas × prompt blocks. `examples/route_scale_bench.rs` measures the
worst case: a 1.1MiB prompt fully warm on every replica, so no walk stops
early.

| replicas | 1 | 2 | 4 | 10 | 20 | 40 |
|---|---:|---:|---:|---:|---:|---:|
| route, µs (median) | 7 | 17 | 24 | 72 | 146 | 307 |

Fingerprinting the same prompt takes about 2.7ms, outside the lock and
independent of fleet size. At 40 replicas, the lock is held for roughly a
tenth of what each request spends preparing its fingerprints.

## Adding or removing a node

Edit the topology file and recreate the balancer. The engines keep their KV
caches, but the balancer's prefix index is in-process. It starts empty and
relearns from traffic, so expect a short rise in re-prefill after every
recreate. Ordinals follow file order, so append new nodes rather than
reordering. Route journals, the `x-ramjet-upstream` header, and session
affinity all use ordinals.

## Not yet multi-node

- **One active balancer.** Load, prefix index, and health are per process. Two
  active instances would each see only their own load and split each
  session's affinity. A standby behind a floating address is fine.
  Active-active needs shared routing state.
- **Idle drain and engine parking** count warm replicas and parked replicas
  across the whole fleet. `RJ_IDLE_DRAIN_MAX_PARKED` bounds host memory, which
  is a per-node resource.
- **Machine view** scrapes one host agent and requires GPU indices to be
  unique across all upstreams.
- **Snapshot inventory** connects to its companions over Unix sockets, so the
  companions must run on the balancer's host. Direct KV-event subscription
  uses TCP and works across nodes.
