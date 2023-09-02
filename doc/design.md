**Background**  
The currently running nodex1.0 is a synchronization solution based on the database. While it generally functions well, there are several issues:
1. Complex components make maintenance challenging, often requiring manual intervention.
2. Significant invasive modifications to the original geth, making patching during version updates labor-intensive.
3. Modified synchronization logic often leads to synchronization delays.
4. It can only run in archive mode, leading to rapid storage growth and a complex pruning process.
5. RPC calls to the database result in high performance losses.
6. Data synchronization at the database layer is currently stable only for eth, with other implementations like bsc facing significant challenges and data inconsistencies.

To address these issues and drawing from the experience of maintaining nodex1.0, we propose nodex2.0 with the following core objectives:
1. Fewer components for easier maintenance.
2. Stateless or simple state, eliminating the need for kafka and s3.
3. Minimal intrusion into geth for a straightforward implementation.
4. High performance, low cost, and minimized storage and computational requirements.

**Components**  
1. **Leafage-evm** ([GitHub Link](https://github.com/DeBankDeFi/leafage-evm)): A rust-implemented execution client based on revm.
   - No p2p synchronization.
   - Serves as an RPC node, providing the necessary eth_* calls for state nodes.
   - Synchronizes using geth's trace_blockStorageDiff RPC method.
2. **Geth** ([GitHub Link](https://github.com/DeBankDeFi/go-ethereum-debank/tree/leafage)):
   - Synchronizes via p2p.
   - Records storage state changes (storageDiff) for each block.
   - Modified version of geth that provides the trace_blockStorageDiff RPC method, returning storageDiff for each block.

**Design**  
**EVM statedb Interface**  
Based on the revm specification, the statedb interface is defined to:
1. Access account details by address (balance, nonce, etc.).
2. Access code by code hash.
3. Access storage by address and storage index.
4. Obtain block hash by block number.

**Statedb Linkedlist**  
Leafage-evm stores each block's storageDiff in a linked list similar to geth's snaptree. The top represents the latest block's storageDiff, with older data below, and the bottom layer being rockdb. Each state access is essentially accessing a node in the linked list. To retrieve a specific value, a downward search is performed, accessing all storageDiffs from the current node to the bottom node, until the base rockdb is accessed. For each update, the new block's storageDiff becomes the new linked list head, facilitating incremental updates.

**Block Updates**  
Call eth_blockbynum(current num+1) to get the block for the current height +1.
  - If no reorg occurs (current+1 block.parent == current block.hash), call trace_blockStorageDiff to get storageDiff and update the linked list head.
  - If a reorg occurs (current+1 block.parent != current block.hash), call eth_blockbyhash to backtrack to the reorg's fork point. From there, re-synchronize by calling trace_blockStorageDiff to get the storageDiff.

**Data Migration**  
Supports exporting geth's snapshot data in a format usable by leafage-evm. This is used during the first startup without a mirror to avoid starting updates from block num 0.