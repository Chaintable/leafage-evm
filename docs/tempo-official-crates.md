# Tempo 官方协议 crates 接入

日期：2026-09-08。基于正式T11候选4d16f6b叠加开发，独立PR，不等待也不改写PR #150。

## 来源与范围

官方`tempoxyz/tempo@1ec5653e39c97e02807dddf5e7106e979f3ad909`（v1.14.0）的`tempo-contracts`和`tempo-hardfork`，同一Git精确rev、default-features=false、std开启。当前registry SDK1.11.0未包含正式T11日程，不能直接用其自动选叉代替本地T11支持。

只替换经过一致性核验的ABI/事件/错误/地址及分叉日程/常量。保留Leafage本地hardfork兼容enum及REVM36转换，不用upstream Default/latest选择已激活fork；未排期T12不可自动激活。保持原mainnet支持边界，不顺带声明支持testnet/未知chain。

本地precompile执行框架、storage/journal、gas顺序、strict ABI/16MiB限制、请求模型和已暂缓问题不改。仅修正下表列明的ABI入口差异。primitives签名/RLP算法复用属于后续增量，不引入contracts RPC client、hardfork EVM feature、chainspec或完整Tempo执行栈。

## 实施与验证

1. Cargo解析/feature/重复来源检查，确认REVM36与Core1.7.2，明确新增Alloy EIPs版本。工具链最低1.96，Docker默认同步。
2. 接入前保存本地ABI固定golden；逐项对照上游selector、输入/输出、events indexed及errors。确认差异后只保留确有必要的本地兼容定义。
3. hardfork保留Default=T10和Genesis/T0折叠，显式转换并委托上游日程/参数；逐历史时间边界和实际RPC回归。
4. Tempo/跨链/RPC/ABI fixture测试、locked构建及CI双架构验证。包接入不替代T11 pipeline/oracle/24h验收。

## 当前结果

G0解析成功：两个Tempo包同一Git rev且仅std；normal/build/dev图保持REVM36/Alloy EVM0.29.2/Core1.7.2，新增EIPs2.4.1，与原EIPs1.8.2隔离使用。Cargo.lock解析锁46项，包含未启用的optional REVM42/Alloy EVM0.38/serde2.4.1树；它们不参与当前构建。不能把metadata/lock中的可选包误认成运行时依赖。

解析要求共享alloy-rlp/derive从0.3.13→0.3.16、c-kzg从2.1.7→2.1.8，已定向update并纳入非Tempo回归。官方Git解析还获取forge-std/solady/tempo-std三个固定submodule；当前构建图没有Reth。

### ABI核验与行为变化

接入前从4d16f6b导出19接口、420项ABI，固定在`fixtures/abi-4d16f6b.json`；只去掉参数名/internalType，不去掉类型、顺序、tuple形状、indexed、mutability。官方423项，相比原定义增加23项、移除20项（签名/metadata修改按一增一减计）。精确差异另存`abi-v1.14-reviewed-delta.json`，测试逐项核对，不在测试运行时从上游生成期望值。事件schema没有变化；另外固定地址和Solidity enum discriminant，防止存储键/enum值漂移。

| 差异 | 处理及验证 |
|---|---|
| IRolesAuth.hasRole 参数由本地(bytes32,address)改为官方(address,bytes32) | 采用官方selector，移除错误selector；Genesis/T11真实calldata验证管理员/非管理员返回值 |
| Validator V1.changeValidatorStatusByIndex 本地uint256改为官方uint64 | 采用官方selector并补T1门控；Genesis/T1/T11、越界/权限、未激活时畸形calldata均验证 |
| Factory缺少带logoURI的7参数createToken重载 | T5起开放，6参数入口selector仍为0x68130445；复用现有URI校验，先校验再建状态，允许creator与admin不同，事件从token地址发出 |
| Factory重载失败与静态调用 | 无效URI、超长URI在写状态前拒绝；重复token/静态调用拒绝，检查空/非空logo的slot与事件 |
| DEX 10个函数、FeeAMM.getPoolId、TIP20.decimals、Factory.getTokenAddress的view→pure | 仅ABI metadata变化，保留本地view执行包装、gas及静态调用规则 |
| 官方error增减 | 逐项固定差异；旧本地独有的IFeeManager.InsufficientLiquidity/PolicyForbids、ITIPFeeAMM.InvalidCurrency、IStablecoinDEX.InvalidCurrency、ITIP20.SpendingLimitExceeded没有生产调用引用，删除定义不改变当前revert编码；其他接口仍提供运行时实际使用的相同error |
| Rust字段名改变、ClaimReceiptV1::new | 按官方字段名赋值，字节顺序不变；移除同义本地constructor，直接使用官方constructor，保留现有receipt测试 |

hardfork仍保留Default=T10；Genesis映射官方T0，拒绝未实现T12及未来未知variant。主网timestamp查询来自精确rev，所有旧时间边界及nonce参数golden保留。升级rev时必须先更新适配并通过测试，不能依赖官方latest()/Default自动开新叉。

### 本地验证（macOS arm64，Rust/Cargo 1.96.1）

- G0：`cargo check --offline --locked -p leafage-evm-chains -j 2`通过，20.58秒。
- `cargo test --offline --locked -p leafage-evm-types -p leafage-evm-chains --lib -j 2`：chains **657通过、4原有ignored**；types **21通过**。chains含340项Tempo测试；4个ignored需要libstylus，没有删除或新增ignore。
- RPC全量lib：**80通过、1失败**。失败是未修改的`api_impl/utils.rs:test_spawn_blocking_with_cancel`，50ms超时要求精确执行5次10ms sleep，本次实际4；隔离复测也失败。接入前T11验证日志已同样出现80通过/该项实际4失败，不能标为本次新回归或声称全绿。
- RPC `blockx_batch` / `blockx_wire_contract` / `arbitrum_retryable_estimate`：**3+7+1通过**。
- `cargo check --offline --locked --workspace --all-targets -j 2`通过，34.29秒。Linux双架构和Stylus FFI CI状态待补；本次尚未部署服务，不替代T11 writer/Leafage pipeline、oracle和24h验收。

RPC失败补充证据：从新worktree执行接入前验证日志标记的旧测试二进制`leafage_evm_rpc-37dd5af59eae2fe1`，同一用例再次以4≠5失败；旧日志为T11 target下`v114-validation/leafage-cross-chain-tests-r2.log`，本次复现为`target/official-crates/rpc-cancel-baseline-binary.log`。没有修改该通用模块。

原始日志在本worktree的`target/official-crates/`；构建缓存复用T11 worktree的target，未修改其源码/分支。首次专项测试的动态参数fixture误用tuple abi_encode多编码一层offset，按ABI规则改为abi_encode_params后通过；没有放宽decoder或改生产逻辑来迁就测试。

## 后续升级与回退

本分支`feature/tempo-official-crates`基于4d16f6b，新PR以`feature/tempo-t3-t4-adaptation`为base。先保留堆叠关系；T11合并后再调整PR base，不force-push默认分支。

两项依赖必须一起更新精确rev，重做feature树、旧ABI差异、fork时间边界和跨链回归；禁止随意打开contracts.rpc/hardfork.evm。`Cargo.lock`中的optional新引擎不是当前构建依赖，但增加了解析约束和源码获取成本。

回退时通过PR撤销本分支提交，恢复T11的Cargo.lock、工具链声明和本地定义；不改数据库或writer部署。当前未产生线上或开发机运行状态变更。
