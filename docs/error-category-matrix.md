# 错误分类矩阵（ErrorCategory Matrix）

> 目标：错误契约机器可读，默认异常处理器可据此自动触发退避、背压或关闭。
>
> 适用范围：`spark-core` 模块的稳定错误码（参见 `spark_core::error::codes`）。

## 阅读指引

- **来源**：稳定错误码，统一使用 `<域>.<语义>` 格式。
- **分类**：[`ErrorCategory`](../spark-core/src/error.rs) 枚举分支，用于驱动默认策略。
- **单一事实来源**：分类与默认动作集中声明于 [`spark_core::error::category_matrix`](../spark-core/src/error/category_matrix.rs)，文档与测试均以此为准。
- **默认动作**：`ExceptionAutoResponder::on_exception_caught` 在无显式覆盖时执行的行为：
  - `Busy`：向上游广播 `ReadyState::Busy`（表示暂时不可用）；
  - `BudgetExhausted`：广播 `ReadyState::BudgetExhausted`，携带预算快照；
  - `RetryAfter`：广播 `ReadyState::RetryAfter`，附带退避建议；
  - `Close`：调用 `Context::close_graceful` 优雅关闭通道；
  - `Cancel`：标记 `CallContext::cancellation()`，终止剩余逻辑；
  - `None`：默认不产生额外动作，由业务自行处理。
- **可配置策略**：通过 `CoreError::with_category` / `set_category` 覆盖分类，或在 pipeline 中注册自定义 Handler。

## 分类明细

| 来源（错误码） | 分类 | 默认动作 | 说明 | 可配置策略 |
| --- | --- | --- | --- | --- |
| `transport.io` | `Retryable`（150ms，传输链路恢复后重试） | `RetryAfter` + `Busy(Downstream)` | 典型 TCP/QUIC I/O 故障，建议短暂退避。 | 可根据重试策略调整等待窗口。 |
| `transport.timeout` | `Timeout` | `Cancel` | 传输层请求超时，触发调用取消。 | 若需继续等待，可显式改写分类。 |
| `protocol.decode` / `protocol.negotiation` / `protocol.type_mismatch` | `ProtocolViolation` | `Close` | 协议契约被破坏，必须关闭连接。 | 若协议允许纠正，可改写为 `Retryable`。 |
| `protocol.budget_exceeded` | `ResourceExhausted(BudgetKind::Decode)` | `BudgetExhausted` | 解码预算耗尽，触发背压。 | 可在 `CallContext` 注册定制预算。 |
| `runtime.shutdown` | `Cancelled` | `Cancel` | 运行时已进入关闭流程，终止后续逻辑。 | 若需等待收尾，可改写为 `Retryable`。 |
| `cluster.node_unavailable` / `cluster.network_partition` / `cluster.leader_lost` | `Retryable`（250ms） | `RetryAfter` + `Busy(Upstream)` | 集群拓扑暂时不可用，建议延后重试。 | 可根据集群健康状况调整等待。 |
| `cluster.service_not_found` / `app.routing_failed` | `NonRetryable` | `None` | 目标服务不存在或路由失败，需业务人工干预。 | 在具备兜底副本时可标记为 `Retryable`。 |
| `cluster.queue_overflow` | `ResourceExhausted(BudgetKind::Flow)` | `BudgetExhausted` | 集群队列满，触发速率背压。 | 可扩容队列或自定义 `BudgetKind`。 |
| `discovery.stale_read` | `Retryable`（120ms） | `RetryAfter` + `Busy(Upstream)` | 服务发现数据陈旧，等待刷新后再试。 | 可结合监控加长等待时长。 |
| `router.version_conflict` | `ProtocolViolation` | `Close` | 路由版本冲突，需重新协商。 | 若支持兼容模式，可改写为 `Retryable`。 |
| `app.unauthorized` | `Security(SecurityClass::Authorization)` | `Close` | 权限不足，记录安全事件并关闭通道。 | 可通过自定义 Handler 触发补偿。 |
| `app.backpressure_applied` | `Retryable`（180ms） | `RetryAfter` + `Busy(Downstream)` | 业务端主动背压，需遵守等待窗口。 | 可结合速率控制调整等待时间。 |

> **提示**：当新增错误码时，请同步执行：
> 1. 在 [`spark_core::error::category_matrix`](../spark-core/src/error/category_matrix.rs) 中新增矩阵条目；
> 2. 同步更新本文档表格，确保默认动作描述一致；
> 3. 扩展 `spark-core/tests/contracts/error_category_autoresponse.rs`，保证契约测试覆盖新增条目。

