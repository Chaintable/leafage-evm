# Stable deployment and fee support

The Stable large-base-fee support is integrated into `main`, together with the
configurable block-zero RPC fallback. Use a release containing this merge;
`v1.2.88` contains the fallback but predates the Stable fee-width integration.

Stable historical headers contain base fees such as `10^21`. Leafage keeps
those values as `u128` in the header, storage, execution environment and RPC
results. Gas limits remain `u64`. The existing mainnet execution path is reused;
there is no replacement BASEFEE opcode or truncated execution fee.

## Cosmos genesis configuration

Stable's first real block is block 1. Keep `--genesis-number 1` and add
`--rpc-block-zero` when clients need a placeholder response for numeric block 0.
For the Stable network whose genesis timestamp is `1761721200`, append these
arguments to the existing `standalone` command:

```sh
--genesis-number 1 --rpc-block-zero '{"id":"0x0000000000000000000000000000000000000000000000000000000000000000","height":0,"timestamp":1761721200,"parent_id":"0x0000000000000000000000000000000000000000000000000000000000000000","base_fee_per_gas":0,"miner":"0x0000000000000000000000000000000000000000","gas_limit":0,"gas_used":0}'
```

The fallback applies to `getBlockByHeight` and `eth_getBlockByNumber` when block
0 is absent. It does not create archive state or a bundle 0, and does not change
the real initialization data at block 1. A stored block 0 takes precedence.
Configure the instance that should answer the request; a state instance returning
the placeholder no longer reports the missing-block error used for archive retry.

## Implementation

- `leafage-evm-types::RawHeader` retains Alloy 2.5's JSON fields and canonical RLP
  field order, widening only the base fee. Alloy's generic RPC Header/Block and
  the existing S3, bundle and database paths are reused.
- The small execution changes are maintained in pinned source packages under
  `vendor/`; see its README. Fees are `u128`, monetary products are `U256`.
- `eth_baseFee` retains its existing EIP-1559 estimation formula with wider
  intermediates. Results beyond `u128` and oversized block fee overrides fail
  explicitly. This estimator is not a claim about Stable's fee-setting rules.
- Alloy's unused optional transaction trace metadata remains `u64`; an
  unrepresentable value is omitted there only. Execution and public block data
  retain the original fee.

## Compatibility

Old headers in the `u64` range keep their JSON and RLP encodings. The new binary
can read them. Once large fees are written, old binaries cannot read those
records: do not roll back against the same database. State-diff encoding and S3
keys/version are unchanged. Deploy matching readers for large-fee data.

This integration adds fee-width support, not a complete certification of all Stable
chain-specific execution rules. Native Alloy block replay/export remains scoped
to its existing Ethereum/BSC/Arc paths.

## Validation

The compact regression tests cover header JSON/RLP compatibility and precision,
archive overflow rejection, and actual BASEFEE/GASPRICE execution with exact
fee deduction/refund at `10^21 + 1` and `u128::MAX`. Archive v3/v4 round-trip
coverage also uses large fees, including flush/reopen. CLI and HTTP tests verify
block-zero fee precision. RPC regressions check oversized Celo overrides and
ZKsync Era's explicit native `u64` fee boundary.
