# leafage-bench

Benchmark CLI for comparing `eth_call` performance between **leafage-evm** and **geth**.

## Highlights

Under stress testing with ramped concurrency from 100 to 10,000 concurrent connections, **leafage-evm delivers up to 58% higher throughput and 42% lower tail latency compared to geth**, while maintaining **0% error rate** across all concurrency levels.

| | leafage-evm | geth | delta%          |
|---|---|---|-----------------|
| **Peak QPS** | **12,159** (@ 10k concurrency) | **8,267** (@ 2k concurrency) | **+47%**        |
| **p50 latency @ 10k** | 109.55 ms | 141.59 ms | **+23% better** |
| **p99 latency @ 10k** | 143.58 ms | 219.38 ms | **+35% better** |
| **Error rate** | 0.00% | 0.00% | —               |

leafage-evm scales linearly up to ~12k QPS and remains stable even at 10,000 concurrent connections, while geth plateaus around 8k QPS at 2,000 concurrency and degrades under higher load.

---

## Build

```bash
cargo build --release -p leafage-bench
```

---

## Usage

The CLI has two top-level sub-commands: `run` and `inspect`.

`run` itself has two modes:

- **`run bench`** — Fixed-concurrency benchmark: run N rounds and report latency / QPS.
- **`run stress`** — Stress-test: ramp concurrency to find the maximum sustainable QPS.

### Common parameters (shared by `bench` and `stress`)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--corpus` / `-c` | *(required)* | Path to the corpus JSON file |
| `--target` | *(required)* | Primary RPC endpoint URL (leafage-evm) |
| `--compare` | - | Comparison RPC endpoint URL (geth) |
| `--label` | all | Only run cases with this complexity label: `L1`, `L2`, `L3` |
| `--requests` | corpus size | Total requests per endpoint per round |
| `--rounds` | 1 | Number of benchmark rounds (per concurrency level for stress) |
| `--seed` | - | Shuffle seed for corpus ordering |

All requests use `latest` as the block tag. The per-request RPC timeout is 30 seconds.

### `run bench` — Fixed-concurrency benchmark

```bash
./target/release/leafage-bench run bench \
  --corpus bin/leafage-bench/corpus/corpus.json \
  --target http://leafage-evm:8555 \
  --compare http://geth:8545 \
  --concurrency 10 \
  --rounds 20
```

Additional parameters for `bench`:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--concurrency` | 10 | Number of concurrent requests per endpoint |
| `--output-dir` | - | Directory for export files (`summary.json`, `verbose.json`) |
| `--verbose` | false | Write per-request details to `verbose.json` (requires `--output-dir`) |

**Console output**: After each round, a latency table (p50 / p90 / p95 / p99 / p99.9) broken down by tier (L1 / L2 / L3) is printed to stdout. When `--compare` is set, a side-by-side comparison table is shown. For multi-round runs, an aggregated report (mean ± stddev across rounds) is printed at the end.

**File output** (requires `--output-dir`):

| File | Written when | Contents |
|------|-------------|----------|
| `summary.json` | always | Run metadata, per-round statistics, aggregated statistics (multi-round only) |
| `verbose.json` | `--verbose` is set | Per-request details: case ID, label, latency, return value / error |

### `run stress` — Stress test

```bash
./target/release/leafage-bench run stress \
  --corpus bin/leafage-bench/corpus/corpus.json \
  --target http://leafage-evm:8555 \
  --compare http://geth:8545 \
  --concurrency-levels 100,200,500,1000,2000 \
  --rounds 3
```

Additional parameters for `stress`:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--concurrency-levels` | `100,200,500,1000,2000` | Comma-separated list of concurrency levels to ramp through |
| `--max-error-rate` | 1.0 | Maximum tolerable error rate (%). When exceeded, the ramp stops for that endpoint |

The stress test runs each concurrency level in order, executing `--rounds` rounds per level. After all levels are complete, a summary table and a delta comparison table are printed. The delta table shows how much better/worse the target is relative to compare at each concurrency level (`+N%` = target is better).

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

Both geth and leafage-evm ran on **separate** AWS EC2 `i3en.2xlarge` instances with identical specs:

| | |
|---|---|
| **Instance type** | `i3en.2xlarge` |
| **CPU** | Intel Xeon Platinum 8259CL @ 2.50 GHz (4 cores / 8 vCPUs, Hyper-Threading) |
| **L3 cache** | 35.75 MiB |
| **Memory** | 64 GiB |
| **Storage** | EBS (IOPS 3000, throughput 300 MB/s) |

---

## Benchmark Results

### Stress Test (Concurrency Ramp: 100 → 10,000)

> **Command**:
> ```bash
> cargo run --bin leafage-bench -- run stress \
>   --corpus ./bin/leafage-bench/corpus/corpus.json \
>   --target http://<leafage-evm>:8545 \
>   --compare http://<geth>:8545 \
>   --concurrency-levels=100,200,500,1000,2000,5000,10000 \
>   --seed=20 \
>   --rounds=10 \
>   --requests=2000
> ```

Each concurrency level was run for 10 rounds (2,000 requests per round). Values are mean ± stddev.

#### Summary Table

| Concurrency | Endpoint | QPS (mean ± std) | error% | p50 ms | p95 ms | p99 ms | p999 ms |
|---|---|---|---|---|---|---|---|
| 100 | leafage-evm | 1,368 ± 44 | 0.00% | 69.55 ± 1.02 | 85.95 ± 14.01 | 104.70 ± 43.01 | 127.24 ± 45.46 |
| 100 | geth | 1,393 ± 19 | 0.00% | 68.54 ± 0.97 | 80.00 ± 6.64 | 90.90 ± 25.42 | 120.61 ± 25.59 |
| 200 | leafage-evm | 2,509 ± 80 | 0.00% | 76.33 ± 3.56 | 89.72 ± 8.66 | 95.49 ± 16.33 | 121.82 ± 17.28 |
| 200 | geth | 2,528 ± 56 | 0.00% | 74.54 ± 2.37 | 90.81 ± 15.26 | 95.97 ± 17.34 | 117.58 ± 16.99 |
| 500 | leafage-evm | 5,289 ± 237 | 0.00% | 85.14 ± 6.30 | 102.48 ± 13.39 | 106.55 ± 12.98 | 130.19 ± 10.50 |
| 500 | geth | 5,335 ± 245 | 0.00% | 81.99 ± 7.63 | 106.22 ± 14.10 | 112.72 ± 13.28 | 121.92 ± 13.25 |
| 1000 | **leafage-evm** | **8,842 ± 863** | 0.00% | 92.52 ± 6.25 | 113.03 ± 22.24 | 115.29 ± 22.27 | 138.39 ± 17.46 |
| 1000 | geth | 7,206 ± 1,167 | 0.00% | 101.85 ± 13.83 | 148.03 ± 28.38 | 157.81 ± 32.65 | 189.08 ± 93.43 |
| 2000 | **leafage-evm** | **11,443 ± 1,331** | 0.00% | 112.12 ± 15.34 | 142.89 ± 18.23 | 146.35 ± 18.12 | 160.59 ± 20.98 |
| 2000 | geth | 8,267 ± 2,415 | 0.00% | 138.58 ± 25.18 | 194.31 ± 40.33 | 203.15 ± 42.78 | 257.12 ± 119.24 |
| 5000 | **leafage-evm** | **11,430 ± 813** | 0.00% | 111.34 ± 8.04 | 140.20 ± 10.57 | 142.85 ± 11.15 | 161.34 ± 12.75 |
| 5000 | geth | 8,190 ± 2,822 | 0.00% | 126.08 ± 3.27 | 181.86 ± 24.48 | 188.03 ± 27.34 | 246.77 ± 123.39 |
| 10000 | **leafage-evm** | **12,159 ± 825** | 0.00% | 109.55 ± 8.90 | 138.92 ± 9.41 | 143.58 ± 11.32 | 154.66 ± 13.15 |
| 10000 | geth | 7,705 ± 2,525 | 0.00% | 141.59 ± 24.14 | 207.05 ± 47.42 | 219.38 ± 51.73 | 266.70 ± 111.78 |

#### Peak Sustainable QPS

| | Max QPS | At Concurrency | p50 | p99 | error% |
|---|---|---|---|---|---|
|  **leafage-evm** | **12,159** | 10,000 | 109.55 ms | 143.58 ms | 0.00% |
| geth | 8,267 | 2,000 | 138.58 ms | 203.15 ms | 0.00% |

#### Delta (leafage-evm vs geth)

`+N%` = leafage-evm is better (higher QPS / lower latency)

| Concurrency | QPS delta% | p50 delta% | p95 delta% | p99 delta% | p999 delta% |
|---|---|---|---|---|---|
| 100 | −1.78% | −1.46% | −7.43% | −15.18% | −5.49% |
| 200 | −0.77% | −2.40% | +1.20% | +0.50% | −3.60% |
| 500 | −0.86% | −3.85% | +3.52% | +5.47% | −6.78% |
| **1000** | **+22.70%** | **+9.16%** | **+23.65%** | **+26.94%** | **+26.81%** |
| **2000** | **+38.42%** | **+19.10%** | **+26.46%** | **+27.96%** | **+37.54%** |
| **5000** | **+39.55%** | **+11.69%** | **+22.91%** | **+24.03%** | **+34.62%** |
| **10000** | **+57.80%** | **+22.63%** | **+32.91%** | **+34.55%** | **+42.01%** |

**Key observations**:
- At low concurrency (100–500), both implementations perform nearly identically.
- Starting at **1,000 concurrent connections**, leafage-evm pulls ahead with **+23% higher QPS** and **+27% lower p99 latency**.
- The gap widens with load: at **10,000 concurrency**, leafage-evm achieves **+58% higher QPS** and **+42% lower p999 latency**.
- geth's QPS plateaus at ~8,267 (concurrency=2000) and **degrades** under heavier load, while leafage-evm continues to scale up to **12,159 QPS**.
- leafage-evm's latency variance (stddev) remains tight even at 10k concurrency, indicating more predictable and stable performance.

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
