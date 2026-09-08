# S3 read timeout

- `[2026-09-08][done] 按最小有效改动收敛 PR #228` — 初版扩大到包装类型、额外 CLI、依赖和重复文档；用户要求仅本项目整改，随后确认继续。
  **Decision:** 保留 SDK Client，在现有 utils.rs 增加完整读取超时并显式传 Duration；撤掉 S3Reader、新依赖和 archive-init CLI。仅保留故障、取消、配置测试及一处配置说明，不修改全局规范。
  **Done:** 四处 GET 和一处 LIST 均有超时；JSON s3_read_timeout_secs 为正整数，省略及程序默认均为 60；archive-init 使用固定 60 秒。原有批次重试、JoinSet、状态和 offset 顺序保持不变。以追加提交更新 [PR #228](https://github.com/Chaintable/leafage-evm/pull/228)，不改已推送历史。
- `[2026-09-08][done] 本地验证` — 在 fix-s3-read-timeout worktree 运行完整 binary 测试。
  **Done:** cargo test --locked -p leafage-evm --bin leafage-evm -j 4：16 通过、1 失败、0 跳过；新增 3 项全部通过。mock GET 无响应、body 停滞和 LIST 无响应均在 1 秒时限返回错误；同 key 再次读取恢复；Pending future 在 10ms 超时后释放。结果取代初版 S3Reader 的测试结果，未测量生产读取耗时。
- `[2026-09-08][open] 原有 HTTP updater 测试缺少 RPC` — test_fetch_block_diff 请求 127.0.0.1:3545 返回 ConnectionRefused，对应源码未改。
  **Decision:** 不删除、跳过或伪造测试；PR 明确说明完整测试未全通过。
- `[2026-09-07][decided] bundle 范围请求独立处理` — 最新 main 的分段下载走独立路径。
  **Decision:** 本次配置只覆盖逐对象 GET/LIST；bundle 保持原行为，另行评估 deadline。
- `[2026-09-07][open] 验证生产默认时限` — 上线前需用正常大对象读取耗时验证 60 秒，并验证故障恢复后的持续进块、hash 和 offset；本任务不部署生产。
