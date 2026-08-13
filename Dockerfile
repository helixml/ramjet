# syntax=docker/dockerfile:1.7
ARG RUST_DEPS_IMAGE=ghcr.io/helixml/mini-dynamo:rust-deps-sha256-73812d07a8a087cd8dfc6d3a882ebdc01be55451e465b4faed964881249cc33a
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
LABEL org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"
# dynamo-tokenizers' PCRE2 regex backend is dynamically linked on Debian.
COPY --from=build /lib/x86_64-linux-gnu/libpcre2-8.so.0.11.2 /lib/x86_64-linux-gnu/libpcre2-8.so.0
COPY --from=build /mini-dynamo /mini-dynamo
COPY compat /compat
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
