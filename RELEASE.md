# Releasing ramjet

Releases are immutable GHCR images plus a matching Git tag and GitHub release.
Mutable `rust-edge` tags are development pointers and are never release
artifacts.

## v0.1 checklist

1. Confirm the release commit passes the complete Drone quality pipeline and
   its dependency, load-balancer, and companion publishers.
2. Record the published load-balancer and companion image digests. Verify both
   images carry `org.opencontainers.image.version=0.1.0` and the release Git
   revision.
3. Deploy the load balancer by immutable digest on node06. Do not restart either
   inference engine. Confirm `/health` reports both replicas healthy and the
   `ds4proxy_upstream_up` metrics are 1 for both replicas.
4. Run cancellation, health/failover, locality, concurrent-same-app, and c24
   aggregate gates. Run one Helix workflow correctness check when a valid
   scoped test credential is available. Record results in `EXPERIMENTS.md`.
5. Roll back the load balancer immediately if correctness, 5xx, TTFT, or
   throughput regresses. The previous immutable digest is the rollback target.
6. After acceptance, create annotated tag `v0.1.0` on the exact deployed commit.
   Publish GitHub release notes from `CHANGELOG.md`, including both immutable
   image digests and the rollback digest.

Do not tag a commit merely because its CI pipeline is green. The tag follows
node06 acceptance so source, release notes, and the deployed artifact identify
the same qualified build.
