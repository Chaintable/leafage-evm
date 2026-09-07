# S3 read timeout

- `[2026-09-07][decided] 实现可配置完整 S3 读取超时` — 用户确认方案并要求开始编码；分支 fix/s3-read-timeout 基于 origin/main e5a8e5052a4d3ed38a01e0b3bf20db016521e807，主 checkout 保持原状。
  **Decision:** Kafka/S3 JSON 新增正整数 s3_read_timeout_secs，省略默认 60；完整 GET/send+body 和 LIST 使用同一时限。复用现有 JoinSet 取消及批次重试，不修改状态/offset 顺序。
  **Done:** 实现提交 5983f0de22d18debae602555d09028110a1fe36a，已推送并创建 [PR #228](https://github.com/Chaintable/leafage-evm/pull/228)；未合并或部署生产。
- `[2026-09-07][decided] 最新 main 的 bundle 路径保持独立` — 最新 main 比事故版本多出 bundle/mod.rs 的分段下载及重试实现；已审方案覆盖 utils.rs 的四处 GET 和一处 LIST。
  **Decision:** 本次只保护逐对象读取。bundle 路径不改，不宣称所有下载均受新配置保护；后续单独评估范围请求的 deadline。
- `[2026-09-07][done] 验证读取超时与取消` — 增加配置校验、GET 无响应、body 停滞、LIST 无响应、断连重试恢复和 Pending future 释放测试。
  **Done:** cargo check -p leafage-evm --tests -j 4 通过。实际 s3.rs 的隔离 harness 6/6 通过；完整 binary 测试 20 通过、1 失败（见下项），新增 7 项全部通过，原有 utils/bundle 回归通过。网络测试中 200ms 的总时限中止 send/body，1s 配置允许同一慢响应完成；未测量生产对象耗时。
- `[2026-09-07][open] 确定生产时限` — 60 秒是候选默认值，需上线前依据正常大对象读取耗时验证；本任务不部署生产。
- `[2026-09-07][decided] 将时限绑定到 S3Reader` — 公共读取被初始化、同步、预热、rewind 和 archive-init 复用，逐层新增 Duration 参数会影响所有调用链。
  **Decision:** 新增小型 S3Reader 保存 SDK Client 和时限，clone 保留配置；utils.rs 统一调用 GET/LIST 方法。KafkaS3Config 使用 NonZeroU64 并显式实现 Default=60，archive-init 提供同名 CLI 参数。bundle 调用显式取原 SDK Client。
- `[2026-09-07][decided] 本地验证` — lihe-dev 有 Cargo，但缺 protoc/cmake 等完整构建工具；本地有现成依赖与构建缓存。
  **Decision:** 在当前 worktree 内复制 APFS 构建缓存并运行完整 cargo check --tests；同时用包含实际 s3.rs 的隔离 harness 执行网络故障测试，不改依赖源码或跳过失败测试。
- `[2026-09-07][open] 原有 HTTP updater 测试依赖未运行的本地 RPC` — 完整 cargo test --locked -p leafage-evm --bin leafage-evm -j 4 中，updater::http_updater::tests::test_fetch_block_diff 请求 http://127.0.0.1:3545 的固定区块 18022783，返回 ConnectionRefused；对应源码未改动。
  **Decision:** 不删除、跳过或伪造该测试所需服务；在 PR 明确记录该环境限制，不将完整测试标为通过。
