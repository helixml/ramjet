# syntax=docker/dockerfile:1.7
FROM rust:1.95-bookworm AS build
WORKDIR /src

# Build the dependency graph before copying application sources. The resulting
# target directory is an ordinary layer so BuildKit's registry exporter can
# reuse it on an ephemeral CI runner; cache-mount contents are builder-local and
# are not part of an exported cache.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked --bin mini-dynamo \
    && rm -rf src \
    && rm -f target/release/mini-dynamo target/release/deps/mini_dynamo-* \
    && rm -rf target/release/.fingerprint/mini-dynamo-*

COPY src ./src
COPY compat ./compat
RUN cargo build --release --locked --bin mini-dynamo \
    && cp target/release/mini-dynamo /mini-dynamo

FROM gcr.io/distroless/cc-debian12
LABEL org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"
# dynamo-tokenizers' PCRE2 regex backend is dynamically linked on Debian.
COPY --from=build /lib/x86_64-linux-gnu/libpcre2-8.so.0.11.2 /lib/x86_64-linux-gnu/libpcre2-8.so.0
COPY --from=build /mini-dynamo /mini-dynamo
COPY compat /compat
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
