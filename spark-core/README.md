# spark-core

## 契约映射
- **宿主契约定位**：`spark-core` 提供 `CallContext`、`ReadyState`、`CloseReason` 等核心抽象，是所有传输、编解码与可观测性 crate 的公共契约来源。框架内的 `Pipeline`、`HostRuntime` 与 `CodecRegistry` 均通过这些类型对齐行为，确保文档与实现一致。
- **跨 crate 联动**：传输层 (`spark-transport-*`) 通过 `transport::channel` 模块实现 `poll_ready`、`close_graceful` 等接口；扩展编解码器遵循 `codec::Codec` trait；观测与重试策略分别依赖 `observability` 与 `retry` 模块输出的契约。
- **状态机约束**：`status::ready` 模块定义 `ReadyState` 五态矩阵，`pipeline::default_handlers` 把传输层反馈映射为事件流，契约测试 (`crates/spark-contract-tests`) 以此为基准校验实现。

## 错误分类
- `error` 模块公开 `CoreError`、`DomainError`、`ErrorCategory` 等类型：
  - **协议/实现类**：`ImplError` 与 `CoreError` 对应内部 bug、协议违例，契约要求在日志中落入 `implementation.*`/`protocol.*` 族别。
  - **业务类**：`DomainError` 带自定义分类，用于调用方错误或业务可重试情形。
  - **安全/资源类**：`ErrorCategory::{SecurityViolation, ResourceExhausted}` 被自动转译为 ReadyState 与关闭原因，避免重复分类。
- 错误编码集中在 `error::codes`，各模块通过 `IntoCoreError` 派生语义化错误并在观测面保持统一指标。

## 背压语义
- `limits::Budget` 提供 Flow/Concurrent/Queue 等预算类型；`CallContext::budget` 与 `BudgetDecision` 控制消费、透支与回收逻辑。
- `ReadyState` 是背压对外接口：`BusyReason` 描述拥塞源，`RetryAdvice` 给出退避时间，`SubscriptionBudget` 携带剩余额度。`pipeline::default_handlers` 按错误分类映射为 Ready/BudgetExhausted/RetryAfter。
- 传输层通过 `transport::backpressure` trait 报告 `ReadyState`，而 `rt::TaskDriver` 根据 `ReadyCheck` 决定是否调度下一条请求。

## TLS/QUIC 注意事项
- 核心层只定义安全契约：`security::TlsHandshake` 与 `transport::quic` 特征给出握手回调、证书验证挂钩。实际 TLS/QUIC 实现在 `spark-transport-tls`、`spark-transport-quic`。
- 合同明确：
  - TLS/QUIC 渠道必须遵循 `CallContext` 的取消与超时语义，在握手阶段提早退出时应返回 `ErrorCategory::Timeout` 或 `ErrorCategory::SecurityViolation`。
  - 半关闭顺序与 ReadyState 映射需与 `docs/graceful-shutdown-contract.md`、`docs/transport-handshake-negotiation.md` 对齐，以便安全审计。

## 半关闭顺序
- 框架契约要求“写半关闭 → 等待对端确认 → 释放资源”：`transport::channel::Close` trait 强制实现 `close_graceful`/`close_force`，`host::HostRuntime` 在 FIN 后仍保持 `Pending` 状态。
- `docs/graceful-shutdown-contract.md` 定义强制序列：
  1. `CallContext::close_graceful()` 向下游发送 FIN。
  2. 监听 `closed()` future，等待读/写方向均确认半关闭。
  3. 若 `Deadline` 触发，升级为 `close_force()` 并报告 `CloseReason::Timeout`。

## ReadyState 映射表
| 场景 | ReadyState | 触发来源 |
| --- | --- | --- |
| 管道空闲/预算充足 | `Ready` | `ReadyCheck::Ready` 默认分支 |
| 下游线程池/依赖拥塞 | `Busy(BusyReason::downstream/upstream/queue_full/custom)` | `pipeline::default_handlers::ExceptionAutoResponder` 与传输 backpressure |
| 预算耗尽 | `BudgetExhausted(SubscriptionBudget)` | `BudgetDecision::Denied` 与 `limits::SubscriptionBudget` 快照 |
| 需要退避 | `RetryAfter(RetryAdvice)` | 重试策略或传输拥塞，含重放防御 |
| 等待外部事件 | `Pending(ReadyState)` | `poll_ready` 未准备好，携带预期下一状态 |

## 超时/取消来源（CallContext）
- `CallContext` 封装 `Deadline`、`Cancellation`：
  - `Deadline` 来自宿主构造或重试策略；超时会转译为 `ErrorCategory::Timeout` 并驱动 `close_force`。
  - `Cancellation` 通过 `CallContext::cancellation_token()` 暴露，传输层与业务处理器均需监听以确保半关闭阶段可被打断。
- 所有传输/编解码 crate 通过 `ExecutionContext` 共享同一 `CallContext`，确保计时器与取消请求单点生效。
