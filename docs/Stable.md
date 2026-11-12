# Stable development

`stable` is the baseline branch with unchanged, pinned execution dependencies; `feat/stable-base-fee-u128` contains the initial
large-base-fee support. Changes target `stable`, not `main`.

Stable historical headers contain base fees such as `10^21`. This branch keeps
those values as `u128` in the header, storage, execution environment and RPC
results. Gas limits remain `u64`. The existing mainnet execution path is reused;
there is no replacement BASEFEE opcode or truncated execution fee.

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

This branch adds fee-width support, not a complete certification of all Stable
chain-specific execution rules. Native Alloy block replay/export remains scoped
to its existing Ethereum/BSC paths.

## Validation

The compact regression tests cover header JSON/RLP compatibility and precision,
archive overflow rejection, and actual BASEFEE/GASPRICE execution with exact
fee deduction/refund at `10^21 + 1` and `u128::MAX`. Existing archive round-trip
coverage also uses a large fee. No full block fixture is needed.

Local validation on the development branch: 1,417 tests passed and 11 existing
tests were ignored. The existing HTTP-updater integration test requires an
Ethereum RPC at `127.0.0.1:3545` and was excluded after its connection failed.
Workspace formatting and strict clippy still report the same baseline issues;
all-target clippy reports no diagnostics on the changed lines.
