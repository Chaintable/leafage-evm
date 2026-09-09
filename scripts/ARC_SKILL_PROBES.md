# Arc comparison probes, 2026-09-09

## Current call-like corpus (2026-09-10)

`arc_skill_call_like.py` tests post-state H / environment H. It cross-checks
writer `trace_callMany(H)` against `pre_traceMany(H+1, overrides=H)`, then compares
Leafage. The latter writer method starts at the canonical parent state H and
does not apply appended-block lifecycle hooks. Do not use `eth_simulateV1` as a
drop-in oracle for these requests; the original corpus below preserves that
earlier, non-equivalent comparison and is not the current acceptance gate.

This is a frozen Arc-specific suite, not a portable runner. It requires the
reviewed local `verify_arc_queries.py` fixture module and its adjacent
`../fixtures/arc-pq-valid.json` vector. The local task directory is
`/Users/lihe/code/task_arc/local/leafage-arc-verification/scripts`. The manifest
records the fixture module hash; the task evidence checksum file also pins the
PQ vector. Do not substitute fixtures without revalidating the corpus.

```sh
uv run --no-project scripts/arc_skill_call_like.py --fixtures /path/to/fixture/scripts --round probe --output /path/to/new/probe
uv run --no-project scripts/arc_skill_call_like.py --fixtures /path/to/fixture/scripts --round probe --fee-mode zero --output /path/to/new/zero
uv run --no-project scripts/arc_skill_call_like.py --fixtures /path/to/fixture/scripts --round directed --output /path/to/new/directed
uv run --no-project scripts/arc_skill_authority_roles.py --output /path/to/new/roles
uv run --no-project scripts/arc_skill_call_like.py --fixtures /path/to/fixture/scripts --round usdc-authority --output /path/to/new/authority
PYTHONDONTWRITEBYTECODE=1 uv run --no-project -m unittest discover -s scripts -p test_arc_skill_call_like.py -v
```

All RPC endpoints, base block/hash and actors are intentionally fixed. Positive
fee mode uses gasPrice=20 gwei; zero mode explicitly sets Leafage baseFee=0 to
match writer call preparation. No number rewriting, balance injection, code or
storage overrides are performed. Use `--plan-only` to save a manifest without
RPC. `--replay-from /path/to/old/run` evaluates exact saved requests offline;
request/side mismatches abort. Replay outputs go to a new directory and do not
add RPC or unique-transaction counts.

| Round | Cases | Request cap | Time cap |
|---|---:|---:|---:|
| probe (positive or zero) | 6 | 40 | 120 s |
| directed | 40 | 300 | 600 s |
| authority (original blocked entry) | 6 | 80 | 180 s |
| usdc-authority | 6 | 80 | 180 s |
| authority_roles.py (read-only role discovery) | 7 getters | 18 | 120 s |

Every request has a 10-second timeout. Exit 0 means the round/report completed,
not PASS. Inspect `summary.json`, `assertions.json` and raw responses. Raw FAIL
remains FAIL even when narrowly matched to an existing accepted inspector or
error-format limitation. Gas differences have no tolerance. The comparator
tests reject mutated gas, missing successful children, unknown emitters and
ordinary log differences.

The original `authority` corpus intentionally remains reproducible: direct
`from=USDC` is rejected because USDC is blocklisted at H. Those six cases are
BLOCKED for their intended writes, not successful permission tests. The
`usdc-authority` corpus instead uses the actual masterMinter/blacklister roles
read from both nodes to call USDC. Minter configuration, mint/burn and blocklist
changes exist only in per-request simulation state. No keys or broadcasts are
used; this does not verify real signing authority. Revalidate roles if changing
the block. USDC-level rejection does not prove deeper NCA boundary coverage.

Failed-prefix status and exact subsequent fill are checked. This deployed
trace_callMany retains reverted output and frame gas; pre_traceMany instead
returns gasUsed=0 as an error placeholder. For the frozen REVERT cases without
authorizationList, total gas is reconstructed from tx gas minus root frame gas
limit plus frame gas used, with the EIP-7623 calldata floor applied. Nine saved
first-item failures cross-check this against debug_traceCall; the reconstruction
is not generalized to Halt or authorization refunds. See
https://eips.ethereum.org/EIPS/eip-7623 for the floor formula. Failed output and
root fields are also compared. Full failed-child trees are not claimed equal.

## Original layer-0 diagnostic corpus

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
