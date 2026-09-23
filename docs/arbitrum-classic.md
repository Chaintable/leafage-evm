# Arbitrum Classic historical calls

Use the shared Arbitrum executor with an explicit Classic mode:

```sh
leafage-evm standalone ... --archive --evm-type=arbitrum \
  --evm-custom-config='{"execution_mode":"classic"}'
```

Omitting `execution_mode` preserves Nitro behavior. The chain ID (42161) cannot distinguish Classic from Nitro. Classic mode pins the pre-Shanghai opcode baseline internally; do not try to enable it with `--spec-id` alone. It applies to execution during normal synchronization as well as to databases populated by `archive-init`.

Keep the producer and reader's StateDiff addressing aligned. For the existing Classic dataset, use `"state_diff_key":"block-hash"` in the Kafka/S3 configuration, or `archive-init --statediff-key block-hash` for bulk initialization. This is independent of EVM mode. The mode requires no new blockfile or StateDiff fields.

## Supported scope

This is **account-state-only call simulation**, not a replacement for the Classic AVM execution client. `eth_call`, `eth_multiCall`, `debank_contractMultiCall`, `pre_traceCall`, and `pre_traceMany` use the existing Arbitrum EVM, account journal, call stack, inspectors and historical state database.

The supported subset includes ordinary EVM contract reads and simulated storage/value changes; CALL, STATICCALL and DELEGATECALL between ordinary contracts; standard Ethereum precompiles; and Classic-specific handling of:

- COINBASE = zero; DIFFICULTY = 2500000000000000; TIMESTAMP from the historical header; CHAINID from configuration.
- RETURNDATACOPY zero-filling beyond the return buffer, including an empty buffer.
- ArbSys `arbBlockNumber`, `arbChainID`, `isTopLevelCall`, `getTransactionCount`, and zero-caller-only `getStorageAt`.
- Classic eth_call's unpriced ContractTransaction does not increment the caller nonce or charge Nitro poster fees.
- ArbInfo at 0x65 executes its actual historical EVM bytecode. Classic ArbOwner is 0x6b; Nitro-only 0x70+ are not intercepted.

## Limits

Execution touching unavailable semantics returns an explicit `Arbitrum Classic:` error, even from a nested low-level call that would otherwise swallow a child failure:

- NUMBER (L1 height), BLOCKHASH (Classic's private inbox-derived hash history), GASLIMIT (private ArbOS pool limit), GASPRICE and BASEFEE (private ArbGas price). The L2 header's fields are not substitutes.
- Other ArbOS builtins, including pricing, retryables, address/function tables, owner operations, ArbOS version and caller alias queries. Historical private ArbOS state and version are not in account StateDiffs.
- Contract creation/destruction, whose Classic account lifecycle is not implemented in this mode.
- Typed transactions, nonzero gasPrice, and `debank_estimateGas`.

Unknown or unsupported builtin selectors also return an explicit error rather than running Nitro code against Classic state. This error is a capability limit, not evidence that the original Classic call reverts.

Gas in calls/traces/multicall remains **revm resource accounting, not Classic ArbGas**. GAS, gas-sensitive branches and low-gas calls can differ from AVM even if no missing-data instruction executes. Use the archive Classic node for exact gas, estimates, transaction replay or these unsupported paths. PUSH0 and later Ethereum opcodes are not enabled. Full transaction simulation/replay fidelity is not claimed.

## Verification

```sh
cargo test -p leafage-evm-chains --lib arbitrum::
cargo test -p leafage-evm-rpc --lib arbitrum::
python3 scripts/test_arbitrum_classic.py \
  --classic http://127.0.0.1:8545 --leafage http://127.0.0.1:8659 \
  --heights 156000,1107013,4198902 --report /tmp/classic-comparison.json
```

The differential script issues only read/simulation RPCs. Both nodes must have archive state at the selected heights. It compares return bytes and historical balance/nonce/code/storage, not AVM gas usage. Synthetic opcode probes use RPC state overrides. It also checks explicit missing-data failures, multicall output, tracing and estimate rejection. The JSON report retains failures for debugging.
