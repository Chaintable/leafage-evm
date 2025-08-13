FROM rust:1.88.0-bookworm AS builder

ARG features
ENV features=$features
RUN echo $features

WORKDIR /app
COPY . .
RUN apt-get update -y && apt-get install -y libclang-dev protobuf-compiler
ENV RUSTFLAGS="-C force-frame-pointers=yes -C debuginfo=1 --cfg tokio_unstable"
RUN cargo build --release --features "$features"


FROM ubuntu:22.04
RUN apt-get update && apt-get install -y ca-certificates wget libjemalloc-dev graphviz binutils ghostscript
WORKDIR /app
COPY --from=builder /app/target/release/leafage-evm .
ENTRYPOINT  ["/app/leafage-evm"]
