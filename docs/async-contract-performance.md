# BoxFuture/BoxStream 性能评估报告

## 1. 测试目标
- 量化对象安全包装（`BoxFuture`、`BoxStream`）相对于泛型/GAT 风格实现的额外开销。
- 验证在高并发建连（Future）与大量事件订阅（Stream）场景下，框架默认契约的性能足够稳定。

## 2. 测试环境
- 工具链：`rustc 1.89`（仓库指定 `rust-toolchain`）。
- 命令：`cargo bench --workspace -- --quick`。
- 关键基准：`spark-core/benches/async_contract_overhead.rs`。

## 3. 结果摘要
| 项目 | 迭代次数 (`--quick`) | 内联实现平均耗时 | Box 实现平均耗时 | 相对差异 |
| --- | --- | --- | --- | --- |
| Future 轮询 | 200,000 次 | 6.23 ns/次 | 6.09 ns/次 | -0.23 ns（≈ -0.9%） |
| Stream 轮询 | 50,000 次 | 6.39 ns/次 | 6.63 ns/次 | +0.24 ns（≈ +3.8%） |

> 数据源：`async_contract_overhead` 基准输出。【e8841c†L4-L13】

## 4. 方法说明
- **去优化措施**：基准中的 `ReadyFuture` 与 `ReadyStream` 在 `poll` 阶段写入 `AtomicU64`，同时调用 `noop_waker().wake_by_ref()`，确保编译器不会将循环折叠为常量表达式。
- **背景模拟**：
  - Future 部分模拟 `TransportFactory::connect` 在大量建连流程中快速返回的场景。
  - Stream 部分模拟 `ClusterMembership::subscribe` 将事件广播给多个消费者。

## 5. 结论
- Box 化 Future 的平均耗时与泛型实现差异不足 1%，说明在主流程中无需为微优化引入额外复杂度。
- Box 化 Stream 的额外成本约 3.8%，可通过批量事件或背压策略轻松摊平；相比其带来的扩展性（可热插拔集群实现），该成本完全可接受。
- 文档已经在 `TransportFactory`、`ClusterMembership`、`BoxFuture`、`BoxStream` 等核心契约中标注性能结论，提醒使用者评估扩展策略时参考基准数据。

## 6. 后续建议
- 如需验证不同优化配置（LTO、ThinLTO、`-C target-cpu=native`），可在 CI 中新增矩阵并复用同一基准。
- 若未来引入 GAT 返回类型，可将基准扩展为比较三种实现（Box、`impl Trait`、GAT），持续监控差异。
