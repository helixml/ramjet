# Releasing ramjet

Releases are immutable GHCR images plus a matching Git tag and GitHub release.
Mutable `rust-edge` tags are development pointers and are never release
artifacts.

## Checklist

Replace `<version>` with the `Cargo.toml` package version being released.

1. Confirm the release commit passes the complete Drone quality pipeline and
   its dependency, load-balancer, and companion publishers.
2. Record the published load-balancer and companion image digests. Verify both
   images carry `org.opencontainers.image.version=<version>` and the release
   Git revision.
3. Deploy the load balancer by immutable digest on node06, holding
   `/run/lock/ramjet-node06-deployment.lock` for the whole interval. Do not
   restart any inference engine. Confirm `/health` reports every replica
   healthy and `ds4proxy_upstream_up` is 1 for each.
4. Run the gates that apply to the deployed stack. Where the box is serving
   live traffic and the moratorium has not been lifted for a supervised
   window, request-generating gates are not available: qualify on the existing
   production traffic instead, and say so explicitly rather than reporting a
   gate that was not run. Record results in `EXPERIMENTS.md`.
5. Roll back the load balancer immediately if correctness, 5xx, TTFT, or
   throughput regresses. The previous immutable digest is the rollback target.
6. After acceptance, create annotated tag `v<version>` on the exact deployed
   commit. Publish GitHub release notes from `CHANGELOG.md`, including both
   immutable image digests and the rollback digest.
7. Update the README quickstart pin to the newly published
   `ghcr.io/helixml/ramjet:v<version>@sha256:<digest>`. This follows
   publication because the digest does not exist until the tag pipeline has
   run.

Do not tag a commit merely because its CI pipeline is green. The tag follows
node06 acceptance so source, release notes, and the deployed artifact identify
the same qualified build.
