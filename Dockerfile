FROM rust:1.70.0-bookworm as builder
WORKDIR /app
COPY . .
RUN apt-get update -y && apt-get install -y libclang-dev
ENV RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=1"
RUN cargo build --release


FROM ubuntu:22.04
RUN apt-get update && apt-get install -y ca-certificates wget libjemalloc-dev
WORKDIR /app
COPY --from=builder /app/target/release/leafage-evm .
ENTRYPOINT  ["/app/leafage-evm"]
