# Changelog

## 0.1.0 — 2026-08-13

First public Rust release.

### Stable serving surface

- OpenAI-compatible streaming reverse proxy with request sanitization and
  model-context rewriting.
- Prefix-locality plus weighted-load routing across healthy replicas.
- Health-gated failover and a replica-aware `/health` endpoint.
- Immediate upstream cancellation when the downstream client disconnects.
- Prometheus request, TTFT, usage, cache-outcome, route, load, and health
  metrics under the stable `ds4proxy_` prefix.
- Privacy-bounded decision journaling and offline policy replay.
- Bounded local/remote tokenizer observation that always falls back to the
  approximate router.

### Experimental and disabled by default

- Exact vLLM KV-event shadow inventories and placement canaries.
- Authenticated compact snapshot companions and hot engine-attestation
  rotation.
- Production snapshot Compose/Caddy admission artifacts.

These experimental paths cannot affect ordinary routing or health unless an
operator explicitly enables their validated gates.

### Qualification

- 330 Rust unit tests plus 38 integration/adversarial/E2E tests before the
  release metadata cut.
- Node06 8× RTX PRO 6000 serving control: 1,820–1,844 output tok/s at
  c24/max256, with 144/144 successful requests.
- Concurrent same-app throughput improved from 298 to 469 tok/s versus the
  original load-blind behavior, while request preparation is about 10× faster
  than the retired Go implementation at 256KiB–2MiB request sizes.
