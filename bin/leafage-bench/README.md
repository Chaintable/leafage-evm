# leafage-bench

Benchmark CLI for comparing `eth_call` performance between **leafage-evm** and **geth**.

## Highlights

Under stress testing with ramped concurrency from 100 to 10,000 concurrent connections, **leafage-evm delivers up to 68% higher throughput and 47% lower tail latency compared to geth**, while maintaining **0% error rate** across all concurrency levels.

| | leafage-evm | geth | delta%          |
|---|---|---|-----------------|
| **Peak QPS** | **11,206** (@ 5k concurrency) | **7,776** (@ 10k concurrency) | **+44%**        |
| **p50 latency @ 10k** | 120.29 ms | 158.59 ms | **+24% better** |
| **p99 latency @ 10k** | 158.19 ms | 237.47 ms | **+33% better** |
| **Error rate** | 0.00% | 0.00% | —               |
| **CPU usage (mean)** | 6.6% | 42.6% | **−85% lower**  |
| **Memory usage (mean)** | 3.82 GB | 22.03 GB | **−83% lower**  |

leafage-evm remains stable under high concurrency and sustains >11k QPS, while geth stays below 8k QPS and shows significantly higher latency under heavy load. Resource consumption is also dramatically lower: leafage-evm uses ~85% less CPU and ~83% less memory than geth under the same workload.

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
> ./target/release/leafage-bench run stress \
>   --corpus ./bin/leafage-bench/corpus/corpus.json \
>   --target http://<leafage-host>:8555 \
>   --compare http://<geth-host>:8545 \
>   --concurrency-levels 100,200,500,1000,2000,5000,10000 \
>   --rounds 10 \
>   --requests 2000
> ```
>
> System metrics (CPU / memory / load) were collected in parallel via node_exporter scraping
> (see [System Resource Usage](#system-resource-usage-same-run-collected-via-node_exporter) below).

Each concurrency level was run for 10 rounds (2,000 requests per round). Values are mean ± stddev.

#### Summary Table

| Concurrency | Endpoint | QPS (mean ± std) | error% | p50 ms | p95 ms | p99 ms | p999 ms |
|---|---|---|---|---|---|---|---|
| 100 | leafage-evm | 1,223 ± 23 | 0.00% | 77.20 ± 0.64 | 98.51 ± 19.49 | 116.65 ± 30.00 | 152.00 ± 28.83 |
| 100 | geth | 1,198 ± 50 | 0.00% | 78.02 ± 1.08 | 101.77 ± 21.11 | 123.38 ± 36.67 | 193.74 ± 148.75 |
| 200 | leafage-evm | 2,284 ± 68 | 0.00% | 80.87 ± 2.02 | 103.75 ± 13.80 | 110.46 ± 18.81 | 140.10 ± 14.10 |
| 200 | geth | 2,296 ± 61 | 0.00% | 80.07 ± 0.86 | 100.94 ± 11.24 | 110.77 ± 17.69 | 146.50 ± 13.55 |
| 500 | leafage-evm | 4,873 ± 231 | 0.00% | 89.50 ± 4.56 | 109.83 ± 23.04 | 116.27 ± 25.28 | 146.02 ± 19.21 |
| 500 | geth | 4,844 ± 315 | 0.00% | 87.45 ± 3.24 | 121.09 ± 21.29 | 131.10 ± 21.49 | 142.70 ± 19.17 |
| 1000 | **leafage-evm** | **7,425 ± 659** | 0.00% | 105.22 ± 6.46 | 142.54 ± 28.32 | 147.30 ± 29.22 | 163.99 ± 26.53 |
| 1000 | geth | 7,267 ± 727 | 0.00% | 106.46 ± 13.35 | 157.43 ± 25.09 | 175.47 ± 28.59 | 192.11 ± 53.50 |
| 2000 | **leafage-evm** | **10,639 ± 1,151** | 0.00% | 127.48 ± 16.58 | 161.67 ± 19.95 | 163.53 ± 20.06 | 176.97 ± 20.74 |
| 2000 | geth | 6,321 ± 2,403 | 0.00% | 170.04 ± 40.39 | 236.14 ± 73.87 | 244.12 ± 75.72 | 333.72 ± 126.90 |
| 5000 | **leafage-evm** | **11,206 ± 795** | 0.00% | 116.05 ± 3.60 | 148.78 ± 9.72 | 155.37 ± 11.23 | 165.68 ± 12.56 |
| 5000 | geth | 6,778 ± 1,995 | 0.00% | 158.31 ± 18.61 | 216.43 ± 28.07 | 226.02 ± 26.46 | 314.75 ± 120.28 |
| 10000 | **leafage-evm** | **10,801 ± 627** | 0.00% | 120.29 ± 12.58 | 153.98 ± 13.52 | 158.19 ± 12.43 | 169.11 ± 12.23 |
| 10000 | geth | 7,776 ± 2,010 | 0.00% | 158.59 ± 28.17 | 229.57 ± 62.93 | 237.47 ± 63.98 | 269.20 ± 98.49 |

#### Peak Sustainable QPS

| | Max QPS | At Concurrency | p50 | p99 | error% |
|---|---|---|---|---|---|
|  **leafage-evm** | **11,206** | 5,000 | 116.05 ms | 155.37 ms | 0.00% |
| geth | 7,776 | 10,000 | 158.59 ms | 237.47 ms | 0.00% |

#### Delta (leafage-evm vs geth)

`+N%` = leafage-evm is better (higher QPS / lower latency)

| Concurrency | QPS delta% | p50 delta% | p95 delta% | p99 delta% | p999 delta% |
|---|---|---|---|---|---|
| 100 | +2.13% | +1.05% | +3.21% | +5.45% | +21.54% |
| 200 | −0.49% | −0.99% | −2.78% | +0.27% | +4.37% |
| 500 | +0.60% | −2.34% | +9.30% | +11.31% | −2.33% |
| 1000 | +2.17% | +1.16% | +9.46% | +16.05% | +14.64% |
| **2000** | **+68.32%** | **+25.03%** | **+31.53%** | **+33.01%** | **+46.97%** |
| **5000** | **+65.33%** | **+26.69%** | **+31.26%** | **+31.26%** | **+47.36%** |
| **10000** | **+38.90%** | **+24.15%** | **+32.93%** | **+33.39%** | **+37.18%** |

#### System Resource Usage (same run, collected via node_exporter)

**Collection method**: Both hosts expose [node_exporter](https://github.com/prometheus/node_exporter) metrics. A polling script queried each host's `/metrics` endpoint every 3 seconds throughout the run. Metrics were derived as follows:

| Metric | Derivation |
|--------|-----------|
| CPU % | `(ΔΣ non-idle cpu_seconds) / (ΔΣ all cpu_seconds) × 100` between consecutive scrapes |
| Mem used (GB) | `(node_memory_MemTotal_bytes − node_memory_MemAvailable_bytes) / 2³⁰` |
| Load avg | `node_load1` / `node_load5` Prometheus gauge, read directly |

Delta formula: `(geth − leafage) / geth × 100`. Positive = leafage-evm uses fewer resources.

Sampling interval: 3 s · samples: leafage = 23 / geth = 23.

| Metric | leafage-evm (mean / max) | geth (mean / max) | delta% |
|---|---|---|---|
| CPU % | 6.6 / 19.8 | 42.6 / 75.2 | +84.48% |
| Mem used (GB) | 3.82 / 4.06 | 22.03 / 24.88 | +82.66% |
| Mem % | 6.2 / 6.5 | 35.5 / 40.1 | +82.66% |
| Load avg 1m | 0.07 / 0.17 | 3.15 / 4.60 | +97.63% |
| Load avg 5m | 0.02 / 0.04 | 2.85 / 3.20 | +99.34% |

`+N%` means leafage-evm uses fewer resources (`(geth - leafage) / geth`).

**Key observations**:
- At low concurrency (100–1000), the two implementations are close in throughput, with leafage-evm generally better on tail latency.
- From **2,000+ concurrency**, leafage-evm shows a clear lead: **+39% ~ +68% QPS**, with substantially lower p95/p99/p999 latency.
- Peak sustainable throughput in this run is **11,206 QPS** for leafage-evm vs **7,776 QPS** for geth (**+44%**).
- System resource usage is significantly lower on leafage-evm in this run, especially on **CPU** and memory.

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
