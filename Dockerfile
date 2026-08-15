# syntax=docker/dockerfile:1.7
ARG RUST_DEPS_IMAGE=ghcr.io/helixml/mini-dynamo:rust-deps-sha256-7da447db3053012bff0f7070cfe7a354d74406e3919b2b9d0ec360371911078d
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

# The machine-view dashboard builds in its own stage so web/ edits never
# invalidate the Rust layer and Rust edits never re-run npm.
FROM node:24-alpine AS ui
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web ./
RUN npm run build

FROM gcr.io/distroless/cc-debian12
ARG OCI_REVISION
LABEL org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"
LABEL org.opencontainers.image.version="0.1.0"
LABEL org.opencontainers.image.revision="${OCI_REVISION}"
# dynamo-tokenizers' PCRE2 regex backend is dynamically linked on Debian.
COPY --from=build /lib/x86_64-linux-gnu/libpcre2-8.so.0.11.2 /lib/x86_64-linux-gnu/libpcre2-8.so.0
COPY --from=build /mini-dynamo /mini-dynamo
COPY compat /compat
COPY --from=ui /web/dist /ui
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
