# leafage-evm

leafage-evm is a light-weight evm excuter that only retains the state of the most recent 64 blocks, which can only be updated through a specific json-rpc interface.

leafage-evm is designed as an eth client that only retains the  the most recent state. It doesn't support the complete eth json-rpc. Its core objective is to offer state-related RPCs like `eth_call` with minimal storage and computational resources. See [design.md](doc/design.md) for details.

## Features

- Support JSON-RPC 
    - [x] eth_call
    - [x] eth_blockNumber
    - [x] eth_getBalance
    - [x] eth_getBlockByHash
    - [x] eth_getBlockByNumber
    - [x] eth_getCode
    - [x] eth_getStorageAt
    - [x] eth_getTransactionCount
    - [x] eth_chainId

- Update by `leafage_blockDiff` RPC, which returns the storage diff of a given block on the parent block's state, See [leafage_blockDiff.md](doc/leafage_blockDiff.md) for details.

- Support Migrate data from geth's state snapshot

- Plan to support different database backends, including rocksdb, mdbx etc.