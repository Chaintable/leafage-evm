# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.93.0

# libstylus.so, built from Chaintable/nitro and published as a COPY-only image.
# Pinned to a revision on purpose: moduleHash and the FFI ABI are tied to the
# nitro commit the library came from, so a floating tag would silently drift
# activation results away from the writer. Bump this deliberately, together
# with a re-verified Stylus diff.
ARG LIBSTYLUS_REV=f7c0ab5

FROM public.ecr.aws/b2h7a5c4/chaintable/libstylus:${LIBSTYLUS_REV} AS libstylus

FROM rust:${RUST_VERSION}-bookworm AS chef

RUN apt-get update && apt-get install -y --no-install-recommends \
        libclang-dev \
        protobuf-compiler \
        lld \
        clang \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked --version 0.1.68

WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY bin ./bin
COPY crates ./crates

RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder

# lld 加速链接；force-frame-pointers 供 pprof 栈展开使用。
# debuginfo 走 Cargo.toml 的 profile.release.debug=true，最终由下方 `strip --strip-debug` 去除。
ENV RUSTFLAGS="-C force-frame-pointers=yes -C link-arg=-fuse-ld=lld --cfg tokio_unstable" \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=never

WORKDIR /app

COPY --from=planner /app/recipe.json recipe.json

# 关键：不用 --mount=type=cache。
# cargo-chef 的 layer cache 模式依赖 target/ 存在 layer 文件系统里，
# cache-to type=gha,mode=max 能完整持久化 layer，cross-runner 恢复。
# cache mount 是 daemon-local，不跨 runner（moby/buildkit#1512），在这个场景反而绕过了 layer cache。
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .

# 源码改动但 recipe 没变时，上面 cook 那层命中缓存 → target/ 里 deps 已齐全。
# 这个 RUN 只重编 workspace crates 的增量。
RUN cargo build --release -p leafage-evm \
    && mkdir -p /out \
    && cp /app/target/release/leafage-evm /out/leafage-evm \
    && strip --strip-debug /out/leafage-evm

FROM builder AS stylus-tests

COPY --from=libstylus /libstylus.so /usr/local/lib/libstylus.so

ENV LEAFAGE_ARB_STYLUS_LIB=/usr/local/lib/libstylus.so

RUN cargo test --release -p leafage-evm-chains --lib \
    && cargo test --release -p leafage-evm-chains --lib -- \
        --ignored --test-threads=1

FROM ubuntu:24.04 AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /out/leafage-evm /usr/local/bin/leafage-evm

# The native Stylus runtime, dlopened on the first Stylus call. If it is missing
# or cannot be loaded, only Arbitrum Stylus execution fails with a node error.
COPY --from=libstylus /libstylus.so /usr/local/lib/libstylus.so

ENV RUST_LOG=info \
    LEAFAGE_ARB_STYLUS_LIB=/usr/local/lib/libstylus.so

ENTRYPOINT ["/usr/local/bin/leafage-evm"]
