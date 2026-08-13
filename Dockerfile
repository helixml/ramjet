# syntax=docker/dockerfile:1.7
ARG RUST_DEPS_IMAGE=ghcr.io/helixml/mini-dynamo:rust-deps-sha256-3b4a156b301af9e116eecbd3cc0df2b5a38d43344d859ef40149499f45141cdf
ARG OCI_REVISION=unknown
FROM ${RUST_DEPS_IMAGE} AS build
WORKDIR /src

# Re-copy the manifests so a deliberately overridden or stale dependency base
# cannot hide a mismatch. The default content-keyed base contains every locked
# dependency and makes this build network-independent.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY compat ./compat
RUN cargo build --release --locked --offline --bin mini-dynamo \
    && cp target/release/mini-dynamo /mini-dynamo

FROM gcr.io/distroless/cc-debian12
ARG OCI_REVISION
LABEL org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"
LABEL org.opencontainers.image.version="0.1.0"
LABEL org.opencontainers.image.revision="${OCI_REVISION}"
# dynamo-tokenizers' PCRE2 regex backend is dynamically linked on Debian.
COPY --from=build /lib/x86_64-linux-gnu/libpcre2-8.so.0.11.2 /lib/x86_64-linux-gnu/libpcre2-8.so.0
COPY --from=build /mini-dynamo /mini-dynamo
COPY compat /compat
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
