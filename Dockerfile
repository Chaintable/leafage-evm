FROM rust:1.70.0-bookworm as builder
WORKDIR /app
COPY . .
RUN apt-get update -y && apt-get install -y libclang-dev
RUN cargo build --release


FROM ubuntu:22.04
WORKDIR /app
COPY --from=builder /app/target/release/leafage-evm .
CMD ["./app/leafage-evm"]
