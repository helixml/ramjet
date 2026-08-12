# syntax=docker/dockerfile:1.7
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,id=mini-dynamo-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=mini-dynamo-target,target=/src/target \
    cargo build --release --locked && cp target/release/mini-dynamo /mini-dynamo

FROM gcr.io/distroless/cc-debian12
LABEL org.opencontainers.image.source="https://github.com/helixml/mini-dynamo"
# dynamo-tokenizers' PCRE2 regex backend is dynamically linked on Debian.
COPY --from=build /lib/x86_64-linux-gnu/libpcre2-8.so.0.11.2 /lib/x86_64-linux-gnu/libpcre2-8.so.0
COPY --from=build /mini-dynamo /mini-dynamo
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
