# Tempo nested execution probe

Read-only fixture for comparing Leafage against a Tempo writer. Submit the **creation bytecode**, with no `to`, to simulation RPCs; do not deploy or broadcast it. The constructor creates its child and performs all nested calls within the same simulated transaction.

## Reproduce

Compiled and exercised with solc `0.8.35+commit.47b9dedd`, optimizer enabled, Osaka target:

```sh
solc --optimize --evm-version osaka --combined-json abi,bin,hashes scripts/fixtures/TempoNestedProbe.sol
```

Take `contracts["scripts/fixtures/TempoNestedProbe.sol:TempoNestedProbe"].bin`, append the mode encoded as one 32-byte ABI uint256, and prefix `0x`. Use identical `from`, `data`, `value: "0x0"`, `gas: "0x2faf080"`, and `gasPrice: "0x0"` on both sides at the same fixed block B. The sender and its nonce-derived creation addresses must have suitable initial state; verify the predicted root and child have no code/nonce before the test.

- Writer: `debug_traceCall(tx, B, {tracer: "callTracer", tracerConfig: {withLog: true}})`.
- Writer state: `debug_traceCall(tx, B, {tracer: "prestateTracer", tracerConfig: {diffMode: true}})`.
- Leafage: `simulateTransactions([tx], {block_id: B, type: "Equals"}, {baseFeePerGas: "0x0"})`.

The explicit Leafage base fee matches the verified writer call behavior for zero gas price. Do not override number/time/blockHash or change transaction type to make a comparison pass.

| Mode | Path | Expected root outcome / child slot 0 |
| --- | --- | --- |
| 0 | Nested CREATE, CALL write, STATICCALL read | Success / 7 |
| 1 | Child writes and emits, then reverts; parent catches | Success / 0 |
| 2 | Same child revert, then parent reverts | `OuterFailure()`; neither contract survives |
| 3 | CALL writes child, DELEGATECALL writes parent | Success / 7; parent slot 0 = 9 |
| 4 | Child CALL with 2300 gas | Caught OOG, root success / 0 |
| 5 | STATICCALL attempts SSTORE | Caught static-write violation, root success / 0 |
| 6 | Nested PathUSD `decimals()` precompile read | Success / 7; decimals = 6 |

Mode 5 deliberately consumes nearly all gas forwarded to the forbidden write; a 50M transaction cap leaves enough gas for the parent to finish. It is not a load test.

## Comparison boundaries

Compare transaction total gas with callTracer root `gasUsed`, not root execution-frame gas. Compare retained child frames, including their gas and outputs, and interleaved event positions. Leafage filters failed child subtrees and renumbers retained calls/events; this is not the complete raw callTracer tree.

Trace logs are **not receipt logs**. In mode 2, both endpoints retain three parent trace events although the transaction reverts. Neither event nor successful child CREATE implies committed effects. Writer prestate diff must show no created contracts. Failed root CREATE lacks `to` in callTracer; writer Parity `trace_call`/authorized legacy `trace_callMany` supplies its predicted address for independent checking.

Leafage currently copies the first failed result into remaining sequence positions. A second result after mode 2 is not evidence that a second transaction executed; test this control-flow contract separately. The constructor assertions in successful modes do check selected post-call storage, but are not a full Leafage world-state diff.

These are synthetic probes: they do not increase unique real-transaction coverage or establish full chain acceptance.
