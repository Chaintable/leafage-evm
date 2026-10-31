# Celo L1 replay inputs

Captured on 2026-10-31 from the native Celo mainnet archive on development
machine `nodelab-workbench-6`, RPC port 8665. The native implementation was
`Chaintable/celo-blockchain` commit `f59d067ab`, pull request #7.

Generated with `scripts/export_celo_replay_inputs.py` using only
`eth_getBlockByNumber(height, true)` and `eth_getBlockReceipts(hash)`.
Each gzip contains version 1, the full block and all receipts, including
the system receipt where native Celo supplies it. Gzip timestamps are zero.

Heights: 0, 1, 1000000, 5000000, 6773999, 6774000, 10000000,
11838439, 11838440, 20000000, 21615999, 21616000, 31052160,
31056496, 31056498, 31056499, and 20872750 (a native 0x7c transaction).

The tests independently recover senders and reconstruct transaction/receipt
roots against Celo's header hash. They also reject modified transaction gas,
receipt status and receipt identity. These inputs cover legacy Celo,
Ethereum-compatible legacy, Ethereum dynamic-fee, and both Celo-specific
dynamic-fee encodings (0x7b and 0x7c).
