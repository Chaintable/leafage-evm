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

---

## Test Environment

Both geth and leafage-evm ran on the **same** AWS EC2 `i3en.2xlarge` instance:

| |                                                                        |
|---|------------------------------------------------------------------------|
| **Instance type** | `i3en.2xlarge`                                                         |
| **CPU** | Intel Xeon Platinum 8259CL @ 2.50 GHz (4 cores / 8 vCPUs, Hyper-Threading) |
| **L3 cache** | 35.75 MiB                                                              |
| **Memory** | 64 GiB                                                                 |
| **Storage** | NVMe                                                                   |

---

## Benchmark Results

> **Command**:
> ```bash
> cargo run --bin leafage-bench run \
>   --corpus ./bin/leafage-bench/corpus/corpus.json \
>   --target http://<geth>:8545 \
>   --compare http://<leafage-evm>:8555 \
>   --concurrency=10 \
>   --requests=1000 \
>   --rounds=20 \
>   --seed=20 \
>   --output-dir=bench-result \
>   --verbose
> ```

Aggregated over 20 rounds (mean ± stddev):

### Overall

| Metric   | leafage-evm (target)  | geth (compare)   | delta    |
|----------|-----------------------|------------------|----------|
| QPS      | 121.86 ± 18.03        | 120.32 ± 19.34   | −1.27%   |
| p50 ms   | 67.06 ± 2.53          | 67.39 ± 4.27     | +0.49%   |
| p90 ms   | 133.21 ± 51.26        | 132.00 ± 56.19   | −0.91%   |
| p95 ms   | 165.49 ± 65.54        | 162.78 ± 59.39   | −1.64%   |
| p99 ms   | 236.05 ± 107.31       | 240.29 ± 80.31   | +1.79%   |
| p999 ms  | 291.96 ± 138.18       | 334.62 ± 133.02  | +14.61%  |

### By Label

| Label | Metric  | leafage-evm (target)  | geth (compare)   | delta    |
|-------|---------|-----------------------|------------------|----------|
| L1    | p50 ms  | 66.84 ± 2.71          | 67.32 ± 4.22     | +0.72%   |
| L1    | p95 ms  | 171.39 ± 77.72        | 164.59 ± 60.89   | −3.97%   |
| L1    | p99 ms  | 239.45 ± 110.46       | 264.91 ± 110.86  | +10.63%  |
| L1    | p999 ms | 284.95 ± 142.95       | 324.74 ± 123.99  | +13.96%  |
| L2    | p50 ms  | 67.02 ± 2.33          | 67.43 ± 4.21     | +0.62%   |
| L2    | p95 ms  | 166.13 ± 66.84        | 161.05 ± 55.40   | −3.06%   |
| L2    | p99 ms  | 245.72 ± 118.00       | 251.82 ± 90.72   | +2.48%   |
| L2    | p999 ms | 281.40 ± 129.27       | 324.09 ± 124.32  | +15.17%  |
| L3    | p50 ms  | 67.93 ± 3.16          | 68.16 ± 5.73     | +0.34%   |
| L3    | p95 ms  | 170.08 ± 66.55        | 157.58 ± 65.40   | −7.35%   |
| L3    | p99 ms  | 251.91 ± 123.37       | 262.91 ± 102.56  | +4.37%   |
| L3    | p999 ms | 271.68 ± 134.28       | 309.19 ± 127.58  | +13.81%  |

**Summary**: At p50 both implementations are essentially identical (~67 ms). leafage-evm shows a
consistent tail-latency advantage at p999: **−14 %** overall vs geth, and **−14 % / −15 % / −14 %**
for L1 / L2 / L3 respectively. Error rate was 0 % across all 20 rounds on both endpoints.

---

## Reference Deployment Configuration

The benchmark was run against the following two services.

### beacon (consensus layer)

- **Image**: `sigp/lighthouse:v8.1.1`

### geth (execution layer)

- **Image**: custom debank-patched build based on `v1.16.7`

```
geth
--datadir=/var/data
--syncmode=full
--state.scheme=path
--snapshot=true
--cache=4096
--http
--http.addr=0.0.0.0
--http.port=8545
--http.vhosts=*
--http.corsdomain=*
--http.api=net,web3,eth,admin,debug,txpool,engine
--maxpeers=200
--rpc.allow-unprotected-txs
--allow-insecure-unlock
--rpc.gascap=250000000
--authrpc.addr=0.0.0.0
--authrpc.port=8551
--authrpc.jwtsecret=/var/data/geth/jwtsecret
--authrpc.vhosts=*
--db.engine=pebble
--history.transactions=0
--ws
--ws.addr=0.0.0.0
--ws.port=8546
--ws.api=net,web3,eth,admin
--pprof
--pprof.addr=0.0.0.0
--pprof.port=9260
```

### leafage-evm

- **Version**: `chaintable-v102-debank-14` 

```
leafage-evm
standalone
--db-path=/nodex
--listen-addr=0.0.0.0:8555
--chain-cfg=1
--meta=<meta-endpoint>
--kafka-s3-config=<kafka-s3-config-json>
--etcd-config=<etcd-config-json>
--warmup-tokens=50000
--readiness-addr=0.0.0.0:6000
```

