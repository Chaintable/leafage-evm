**Migrating from Geth Snapshot Storage**

Starting synchronization from the genesis block can be time-consuming.
Therefore, Leafage supports data migration from Geth's snapshot storage.

To migrate data from a Geth instance with snapshot enabled, follow these steps:

1. **Generate Migration File**
   ```bash
   ./geth snapshot dump2 --dumpdb /nodex_backup --datadir /eth/state/geth/ --ancient.prune
   ```

2. **Import Migration File**
   ```bash
   RUST_LOG=info ./leafage-evm migrate --source-path /nodex_backup --db-path /nodex
   ```

**Start Geth with Leafage_storageDiff Support**

1. start a beacon node
2. start a **leafage-patch Geth**
   ([GitHub Link](https://github.com/DeBankDeFi/go-ethereum/tree/leafage-diff-storage))
   with snapshot , statediff and `trace_*` rpc enabled

**Launch Leafage-evm Server**

```bash
cargo build --release 
RUST_LOG=info ./target/release/leafage-evm  standalone --db-path /nodex --listen-addr 0.0.0.0:8545 --rpc-addr http://127.0.0.1:7545
```
