# leafage-evm

leafage-evm is a light-weight evm excuter that only retains the state of the
most recent 64 blocks, which can only be updated through a specific json-rpc
interface.

leafage-evm is designed as an eth client that only retains the the most recent
state. It doesn't support the complete eth json-rpc. Its core objective is to
offer state-related RPCs like `eth_call` with minimal storage and computational
resources.

## Design

See [design.md](doc/design.md) for details.

## Features

- Only Need ~60GB storage for eth mainnet

- Support JSON-RPC
  - [x] eth_call
  - [x] eth_multiCall
  - [x] eth_baseFee
  - [x] eth_blockNumber
  - [x] eth_getBalance
  - [x] eth_getBlockByHash
  - [x] eth_getBlockByNumber
  - [x] eth_getCode
  - [x] eth_getStorageAt
  - [x] eth_getTransactionCount
  - [x] eth_chainId
  - [x] eth_getTransactionByHash
  - [x] eth_getTransactionByBlockHashAndIndex

- Update by `trace_blockStateDiff` RPC, which returns the storage diff of a
  given block on the parent block's state, See
  [trace_blockStateDiff.md](doc/trace_blockStateDiff.md) for details.

- Support Migrate data from geth's state snapshot

- Plan to support different database backends, including rocksdb, mdbx etc.

## Usage

See [usage.md](doc/usage.md) for details.

## Build

### main

main分支可以构建mainnet和op

- mainnet:

```shell
cargo build --release
```

通过.github/workflows/release.yml 和.github/workflows/build.yml构建

- op

```shell
cargo build --release --features=optimism
```

通过.github/workflows/release_op.yml 和.github/workflows/build.yml构建

### bsc

bsc分支可以构建bsc

```shell
cargo build --release
```

通过.github/workflows/release.yml 和.github/workflows/build.yml构建
