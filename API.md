# leafage-evm JSON-RPC API

leafage-evm 是轻量级 EVM 状态查询引擎，对外暴露三组 JSON-RPC 方法。
所有请求均为 HTTP POST，`Content-Type: application/json`。

**Endpoint：** `http://<host>:8545`

---

## 一、eth_ 标准方法

兼容以太坊标准 JSON-RPC，可直接替换 Geth/全节点使用。

| 方法 | 说明 |
|------|------|
| `eth_blockNumber` | 返回当前最新区块号 |
| `eth_chainId` | 返回链 ID |
| `eth_getBalance` | 查询地址 ETH 余额 |
| `eth_getCode` | 查询合约字节码 |
| `eth_getTransactionCount` | 查询地址 nonce |
| `eth_getStorageAt` | 查询合约存储槽 |
| `eth_getBlockByNumber` | 按区块号查询区块信息 |
| `eth_getBlockByHash` | 按区块 hash 查询区块信息 |
| `eth_call` | 执行只读合约调用 |
| `eth_estimateGas` | 估算交易 gas 用量 |
| `eth_baseFee` | 查询指定区块的 base fee（扩展） |
| `eth_multiCall` | 批量执行只读合约调用（扩展） |

```bash
# eth_blockNumber
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'

# eth_chainId
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":1}'

# eth_getBalance
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getBalance","params":["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","latest"],"id":1}'

# eth_getCode
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getCode","params":["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","latest"],"id":1}'

# eth_getTransactionCount
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","latest"],"id":1}'

# eth_getStorageAt
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getStorageAt","params":["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","0x0","latest"],"id":1}'

# eth_getBlockByNumber
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["latest",false],"id":1}'

# eth_getBlockByHash
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_getBlockByHash","params":["<block_hash>",false],"id":1}'

# eth_call
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_call","params":[{"to":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","data":"0x18160ddd"},"latest"],"id":1}'

# eth_estimateGas
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_estimateGas","params":[{"from":"0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","to":"0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","value":"0x1"}],"id":1}'

# eth_baseFee
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_baseFee","params":["latest"],"id":1}'

# eth_multiCall
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"eth_multiCall","params":[[{"to":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","data":"0x18160ddd"}],"latest"],"id":1}'
```

---

## 二、Debank 扩展方法

DeBank 内部使用的扩展方法，提供更便捷的批量查询和模拟能力。方法名无前缀。

| 方法 | 说明 |
|------|------|
| `version` | 返回节点版本信息 |
| `getLatestBlock` | 返回最新区块（DeBank 格式） |
| `getBlockByHeight` | 按高度查询区块（DeBank 格式） |
| `getBlockById` | 按 hash 查询区块（DeBank 格式） |
| `blockIsValid` | 校验区块 hash 是否有效 |
| `getAddressBalance` | 查询地址余额 |
| `getAddressNonce` | 查询地址 nonce |
| `getAddressCode` | 查询合约字节码 |
| `getStorageAt` | 查询合约存储槽 |
| `estimateGas` | 估算 gas（支持指定 block） |
| `contractMultiCall` | 批量合约调用（支持指定 block） |
| `simulateTransactions` | 模拟执行一组交易，返回状态变更 |
| `debankBlock` | 查询区块完整信息（DeBank 格式） |

```bash
# version
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"version","params":[],"id":1}'

# getLatestBlock
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getLatestBlock","params":[],"id":1}'

# getBlockByHeight
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getBlockByHeight","params":["0x183E34E"],"id":1}'

# getBlockById
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getBlockById","params":["<block_hash>"],"id":1}'

# blockIsValid
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"blockIsValid","params":["<block_hash>"],"id":1}'

# getAddressBalance
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getAddressBalance","params":["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","latest"],"id":1}'

# getAddressNonce
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getAddressNonce","params":["0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","latest"],"id":1}'

# getAddressCode
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getAddressCode","params":["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","latest"],"id":1}'

# getStorageAt
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"getStorageAt","params":["0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","0x0","latest"],"id":1}'

# estimateGas
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"estimateGas","params":[{"from":"0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","to":"0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045","value":"0x1"},"latest"],"id":1}'

# contractMultiCall
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"contractMultiCall","params":[{"calls":[{"to":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","data":"0x18160ddd"}],"block_id":"latest"}],"id":1}'

# simulateTransactions
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"simulateTransactions","params":[{"txs":[],"block_id":"latest"}],"id":1}'

# debankBlock
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"debankBlock","params":["latest"],"id":1}'
```

---

## 三、pre_ 方法（交易 trace）

用于在指定区块状态下 trace 交易执行过程，返回详细调用栈。

| 方法 | 说明 |
|------|------|
| `pre_traceCall` | trace 单笔交易 |
| `pre_traceMany` | 批量 trace 多笔交易 |

```bash
# pre_traceCall
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"pre_traceCall","params":[{"to":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","data":"0x18160ddd"},"latest",{}],"id":1}'

# pre_traceMany
curl -s http://<host>:8545 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","method":"pre_traceMany","params":[[{"to":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","data":"0x18160ddd"}],"latest",{}],"id":1}'
```

---

## 注意事项

- leafage-evm **不支持**发送交易相关方法（`eth_sendRawTransaction`、`eth_sendTransaction` 等），仅提供状态查询
- `block_id` 参数支持：`"latest"` / `"earliest"` / 十六进制区块号（如 `"0x183E34E"`）/ 区块 hash
- 集群模式下请求打到 **rpc-proxy**（默认端口 `8545`），由其负载均衡到后端多个 leafage-evm 节点
