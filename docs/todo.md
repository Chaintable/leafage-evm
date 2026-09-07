# OP spec and contract size configuration

- `[2026-09-07][decided] Implement reviewed OP configuration plan` — RISE and Metis readers reject creation simulations accepted by their writers; OP also ignores spec-id.
  **Decision:** Base e5a8e505; safely resolve OP IDs 100–110 with 255 retaining Osaka, construct spec/gas together, independently override code/initcode sizes, support explicit unlimited initcode. No production changes. Full plan and live evidence are in `/Users/lihe/code/task_rise/docs/LEAFAGE_OP_SIZE_LIMIT_PLAN_20260907.md` and `op_cluster_size_limit_evidence_20260907.json`.
  **Decision:** Use Rust 1.96.1 available locally, isolated worktree target with debug info disabled to limit disk use; test the actual OP executor and RPC paths. Code review and a leafage PR are required before delivery.

- `[2026-09-07][done] Configuration and EVM regression tests` — The OP branch now resolves spec IDs and applies independent size overrides.
  **Done:** Three CLI/config tests passed (all 256 u8 values checked); five production-path OP EVM tests passed, including RISE/Metis boundaries, internal CREATE/CREATE2, CLZ and the 3450-gas P256 fork difference. Independent code review found no blocking product issues.

- `[2026-09-07][decided] HTTP test expectations follow actual error channels` — Initial HTTP assertions incorrectly assumed all batch failures were embedded results, and used a display string for a debug-formatted halt.
  **Decision:** Keep production behavior unchanged: transaction validation errors reject the RPC, execution halts populate batch/pre-trace results. Assert the specific size error in each channel. Strengthen successful calls with byte-length/content checks and reuse estimated gas for another call, as requested by review.

- `[2026-09-07][done] HTTP regression and static checks` — Exercise default OP, Jovian plus RISE limits, and Metis size settings through a real local HTTP server.
  **Done:** New HTTP test passed across eth_call, estimateGas (including reusing returned gas), eth_multiCall, pre_traceMany and pre_traceCall; both existing e2e_smoke tests passed. Changed Rust files pass rustfmt check; cargo clippy for both affected packages (lib/bins/tests, locked) completed with existing repository warnings and no warnings in new test files.
  **Done:** Final independent code review passed after the HTTP assertion improvements; no blocking findings remain.

- `[2026-09-07][open] Existing cancellation timing test fails on local macOS` — Full RPC lib suite reports 69 passed / 1 failed at utils.rs:489, expecting exactly five 10ms iterations before a 50ms timeout; actual count is four.
  **Done:** The same test independently fails 4 != 5 on untouched base e5a8e505 in a detached worktree; no cancellation code changed. Keep the test and report this baseline failure in the PR, rather than skip it or change unrelated code.
