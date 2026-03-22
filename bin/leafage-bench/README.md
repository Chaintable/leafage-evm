# leafage-bench

Benchmark CLI for comparing `eth_call` performance between **leafage-evm** and **geth**.

---

## Build

```bash
cargo build --release -p leafage-bench
```

---

## Usage

### `run` — Run the benchmark

```bash
./target/release/leafage-bench run \
  --corpus bin/leafage-bench/corpus/corpus.json \
  --target http://leafage-evm:8545 \
  --compare http://geth:8545
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--corpus` / `-c` | - | Path to the corpus JSON file (required) |
| `--target` | - | Primary RPC endpoint URL (leafage-evm) (required) |
| `--compare` | - | Comparison RPC endpoint URL (geth) |
| `--label` | all | Only run cases with this complexity label: `L1`, `L2`, `L3` |
| `--concurrency` | 10 | Number of concurrent requests per endpoint |
| `--requests` | corpus size | Total requests per endpoint per round |
| `--rounds` | 1 | Number of benchmark rounds |
| `--seed` | - | Shuffle seed for corpus ordering |
| `--output-dir` | - | Directory for export files (`summary.json`, `verbose.json`) |
| `--verbose` | false | Write per-request details to `verbose.json` (requires `--output-dir`) |

All requests use `latest` as the block tag. The per-request RPC timeout is 30 seconds.

**Console output**: After each round, a latency table (p50 / p90 / p95 / p99 / p99.9) broken down by tier (L1 / L2 / L3) is printed to stdout. When `--compare` is set, a side-by-side comparison table is shown. For multi-round runs, an aggregated report (mean ± stddev across rounds) is printed at the end.

**File output** (requires `--output-dir`):

| File | Written when | Contents |
|------|-------------|----------|
| `summary.json` | always | Run metadata, per-round statistics, aggregated statistics (multi-round only) |
| `verbose.json` | `--verbose` is set | Per-request details: case ID, label, latency, return value / error |

### `inspect` — Inspect the corpus

Print summary statistics of the corpus without running any requests:

```bash
./target/release/leafage-bench inspect \
  --corpus bin/leafage-bench/corpus/corpus.json
```

Example output:

```
file        : bin/leafage-bench/corpus/corpus.json
generated   : 2025-01-15T10:00:00Z
format      : v1
seed        : abc123
stage       : balanced
total cases : 700

quotas:
  L1 : quota=300  actual=300
  L2 : quota=300  actual=300
  L3 : quota=100  actual=100

ingest stats:
  requests_received : 50000
  cases_ingested    : 12000
  rpc_objects_found : 48000
```

---

## Corpus

The benchmark corpus (`corpus/corpus.json`) is a curated set of real-world `eth_call` requests
collected from production mirror traffic, de-hotspotted, and balanced across three complexity tiers
(L1 / L2 / L3) to ensure a fair and reproducible comparison.

The file is tracked via **Git LFS**. After cloning the repository, pull the corpus data with:

```bash
git lfs pull
```

If Git LFS is not installed:

```bash
# macOS
brew install git-lfs
git lfs install
git lfs pull
```

---

## How `corpus.json` Was Collected

The corpus was derived from live production `eth_call` traffic captured via HTTP traffic mirroring.
Raw mirror records were parsed to extract embedded JSON-RPC requests (`eth_call`,
`contractMultiCall`, and similar batch methods), which were then exploded into individual
`eth_call` cases and normalised.

To remove hotspot bias — where a single `(selector, contract)` pair can represent 90 %+ of all
production requests — two deterministic capping passes were applied:

- **Group cap**: at most 20 cases per `(label, selector, contract)` group.
- **Selector cap**: at most 300 cases per `(label, selector)` bucket.

After de-hotspotting, a fixed quota was drawn from each complexity tier (L1 / L2 / L3) to form
the final balanced corpus.

---

## Traffic Classification: L1 / L2 / L3

Every case in the corpus carries a `classification.label` field (`L1`, `L2`, or `L3`).
The label reflects the **execution complexity** of the `eth_call` request, assigned by a
signal-based scoring model.

### Score model

Each request starts at score `0`. Signals add or subtract points:

| Signal | Score delta | Condition |
|--------|-------------|-----------|
| `light-selector` | −3 | Selector is a known cheap getter (`balanceOf`, `totalSupply`, `decimals`, `symbol`, `name`, `implementation`) |
| `calldata-4b` | −2 | Calldata is exactly 4 bytes (selector only, no arguments) |
| `calldata-36b` | −1 | Calldata is 36 bytes (selector + one `address` or `uint256` argument) |
| `calldata-68b+` | +2 | Calldata ≥ 68 bytes (two or more arguments) |
| `calldata-100b+` | +1 | Calldata ≥ 100 bytes |
| `calldata-132b+` | +1 | Calldata ≥ 132 bytes (three or more arguments) |
| `has-from` | +1 | Request includes a `from` field (state-dependent call) |
| `portfolio-tag` | +1 | Request originated from a `portfolio` scenario |
| `token-balance-tag` | −1 | Request originated from a `token_balance` scenario |

### Label assignment

| Score | Label | Description |
|-------|-------|-------------|
| ≤ −2 | **L1** | **Lightweight read.** Simple getter with no or one argument, or a known cheap selector (`balanceOf`, `totalSupply`, `decimals`, etc.). Minimal EVM execution; result is typically a single storage slot read. |
| −1 … +1 | **L2** | **Medium complexity.** Unknown or moderately complex selector, standard view call with one or two arguments. Represents the bulk of typical DeFi read traffic. |
| ≥ +2 | **L3** | **Heavy call.** Long calldata, state-dependent call (includes `from`), or a complex view function such as Uniswap V3 `slot0`, liquidity calculations, or portfolio aggregation queries. Exercises the EVM execution path most deeply. |

### Corpus composition (`corpus.json`)

| Label | Cases | Unique selectors | Unique contracts |
|-------|-------|-----------------|-----------------|
| L1 | 300 | ~21 | ~63 |
| L2 | 300 | ~38 | ~246 |
| L3 | 100 | ~13 | ~62 |
| **Total** | **700** | | |

The 700-case balanced corpus is sufficient for a first-version benchmark that reliably detects
performance differences of 5 %+ between leafage-evm and geth.
