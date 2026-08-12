FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked && cp target/release/mini-dynamo /mini-dynamo

FROM gcr.io/distroless/cc-debian12
COPY --from=build /mini-dynamo /mini-dynamo
EXPOSE 8000 9090
ENTRYPOINT ["/mini-dynamo"]
