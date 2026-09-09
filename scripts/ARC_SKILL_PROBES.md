# Arc comparison probes, 2026-09-09

Frozen, read-only layer-0 diagnostic corpus, not a general acceptance suite.
Targets Arc mainnet chain 5042 at block 15818173. Writer must be available on
localhost:39545 and Leafage on localhost:49545; the scripts do not deploy or
start services. The tested deployments were writer 5fb88e2 and Leafage ce695fc,
not the source branch used to store this test tooling.

```sh
uv run --no-project scripts/arc_skill_round0.py --output /path/to/evidence/round0-retry1
uv run --no-project scripts/arc_skill_diagnostic.py /path/to/evidence
uv run --no-project scripts/arc_skill_diagnostic_fix.py /path/to/evidence
uv run --no-project scripts/arc_skill_diagnostic_fix.py /path/to/evidence --system-probe
uv run --no-project scripts/arc_skill_evaluate.py /path/to/evidence
```

Output directories must not already exist. Each collector preserves complete
requests/responses. R0 caps requests at 160 and elapsed time before a request at
240 seconds. Diagnostics issue 10/2/2 requests, with a 10-second request timeout.
The evaluator is offline and expects the original failed transport attempt at
`round0/raw.jsonl` as well as the successful collection at `round0-retry1`.
It writes `assertions.json`; exit 0 means report generation completed, **not**
acceptance PASS. It intentionally retains mismatches and BLOCKED gates.

D0 deliberately preserves the original malformed hex `blockHash` map key.
D1 corrects it to a decimal JSON key. Do not count D0's invalid input as a node
defect. D2 reads EIP-2935 after matching the execution environment, to demonstrate
that writer's appended-block system processing is a separate oracle condition.

This corpus has no unique real-transaction samples. Passing individual output,
gas, trace or event assertions does not imply a complete simulation oracle, new
block import coverage, or post-Zero7/Zero8 mainnet acceptance. No product fix is
included. Raw evidence and the full report remain in the local Arc task workspace.
