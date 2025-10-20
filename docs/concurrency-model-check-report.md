# 并发内存/顺序性验证报告（Miri / Loom 抽样）

## 目标与范围
- **目标**：使用 Miri 与 Loom 对核心并发路径进行抽样验证，提前捕获潜在的未定义行为与顺序性问题。
- **抽样组件**：
  1. `Cancellation`（取消信号传播）
  2. `Budget`（额度扣减与归还）
  3. `InMemoryAuditRecorder`（内存审计日志串行化）
- **工具链**：`cargo +nightly miri test`、`RUSTFLAGS="--cfg loom" cargo test --features loom-model --test loom_concurrency`

## Loom 模型检查
- **测试入口**：`spark-core/tests/loom_concurrency.rs`
- **覆盖要点**：
  - **取消信号可见性**：验证 `Cancellation::cancel` 的释放语义与子节点可见性，避免遗漏 `Ordering` 导致的自旋。
  - **额度竞争**：在 `Budget::try_consume` / `refund` 的交错序列下，保证剩余额度不会出现下溢或超限。
  - **审计互斥串行化**：`InMemoryAuditRecorder::record` 通过 `Mutex` 保证事件顺序；模型检查遍历写入交错以确认哈希链的一致性。
- **运行结果**：在探索深度 3～4 的 Loom 默认配置下，所有模型测试均通过，未发现死锁或竞态行为。
- **注意事项**：
  - 若需增加探索深度，可使用 `LOOM_MAX_PREEMPTIONS` 环境变量，但运行时间会显著增加。
  - 模型测试依赖 `loom-model` 特性，并需要 `--cfg loom` 以启用针对 Loom 的类型别名。

## Miri 运行结论
- **命令**：`cargo +nightly miri test`
- **结果**：所有工作区单元测试在 Miri 下通过，未发现未定义行为。
- **警告**：Miri 报告了多处 `unused_parens` 警告（主要集中在 `spark-core/src/pipeline` 目录），不影响语义，暂未清理。

## 发现与修复
- **`Arc` 与互斥实现分离**：
  - 在 `spark-core/src/contract.rs` 中固定 `Arc` 的具体类型以保留 `Eq/Hash` 派生，避免 Loom 条件编译导致的 trait 派生失败。【F:spark-core/src/contract.rs†L1-L16】
- **审计互斥切换**：
  - `spark-core/src/audit/mod.rs` 在启用 Loom 时改为 `loom::sync::Mutex`，保持与标准构建的行为一致。【F:spark-core/src/audit/mod.rs†L385-L395】
- **并发测试覆盖**：
  - 新增 `spark-core/tests/loom_concurrency.rs`，包含对取消、预算与审计的穷举测试。【F:spark-core/tests/loom_concurrency.rs†L1-L163】

## 后续建议
1. **扩展预算场景**：增加失败重试、组合额度等模型测试，覆盖更多竞争模式。
2. **审计哈希冲突测试**：引入不同 `state_curr_hash` 的案例，确保互斥锁在冲突下仍保持序列化顺序。
3. **警告清理**：针对 Miri 的 `unused_parens` 提示统一修复，提高编译噪音信噪比。

## 运行指引
```bash
# 启动 Loom 模型检查
RUSTFLAGS="--cfg loom" cargo test --features loom-model --test loom_concurrency

# 运行 Miri（需 nightly toolchain，并提前安装 miri 与 rust-src）
cargo +nightly miri test
```
