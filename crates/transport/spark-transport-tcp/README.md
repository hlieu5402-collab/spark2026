# spark-transport-tcp

## 契约映射
- 提供基于 Tokio 的 TCP 传输实现，暴露 `TcpListener`、`TcpChannel` 等类型，直接遵循 `spark-core::transport::channel` 契约。
- `TcpChannel` 实现 `poll_ready`、`read`、`write`、`close_graceful`、`close_force`，并在内部与 `CallContext` 的 `Deadline`、`Cancellation`、`Budget` 对齐。
- `backpressure::TcpBackpressure` 将 socket 状态 (`WouldBlock` 频率、写缓冲剩余容量) 映射为 `ReadyState::{Busy, RetryAfter}`，与上层调度保持一致。

## 错误分类
- 使用 `error::TcpError` 将 OS 错误分类为 `ErrorCategory::{Transport, Timeout, ResourceExhausted, SecurityViolation}` 等，统一上报指标。
- `TcpListener` 在握手失败时会标记 `CloseReason::ProtocolViolation` 或 `SecurityViolation`，确保调用方可根据分类采取补救措施。
- 对于实现缺陷（如内部状态不一致）会返回 `ImplError`，提示开发者修复。

## 背压语义
- `poll_ready` 根据写缓冲可用性返回：
  - 正常 -> `Ready`；
  - `WouldBlock` 持续出现 -> `Busy(BusyReason::downstream())`；
  - RTT 激增或应用层主动降速 -> `RetryAfter`，携带退避时间。
- 结合 `CallContext::budget(BudgetKind::Flow)` 限制写入速率，当预算耗尽时广播 `ReadyState::BudgetExhausted` 并停止发送。
- 读通道在缓冲不足或被动背压时会通知 `ReadyState::Pending`，提示上游暂缓拉取。

## TLS/QUIC 注意事项
- 作为 TLS/QUIC 的基线参考：
  - TLS wrapper (`spark-transport-tls`) 复用相同的半关闭与背压逻辑；
  - 提供连接元数据（对端地址、ALPN 预留位）以供 TLS 握手层复用；
  - 若检测到 TLS 前置握手失败（例如 ALPN 不匹配），应将错误分类为 `SecurityViolation` 并触发 `close_force`。

## 半关闭顺序
- 遵循“写半关闭 → 等待对端确认 → 释放资源”契约：
  1. `close_graceful` 首先调用 `TcpStream::shutdown(Write)` 发送 FIN；
  2. 持续读取直到对端确认读半关闭或 `Deadline` 超时；
  3. 若超时触发 `close_force`，记录 `CloseReason::Timeout` 并关闭 socket。
- `spark-core::transport::ShutdownDirection` 枚举确保调用方显式声明关闭方向，防止顺序错误，实现与 QUIC/TLS 的共用语义。

## ReadyState 映射表
| Socket 状态 | ReadyState | 说明 |
| --- | --- | --- |
| 写缓冲可写 | `Ready` | 可以继续发送数据 |
| 短暂 `WouldBlock` | `Pending` | 等待下一次可写事件 |
| 持续 `WouldBlock` 或拥塞 | `Busy(BusyReason::downstream)` | 提醒调度器降低速率 |
| RTT 激增/自适应退避 | `RetryAfter` | `backpressure::TcpBackpressure` 根据采样计算 |
| 预算耗尽 | `BudgetExhausted` | `CallContext::budget` 拒绝继续发送 |
| 握手/安全失败 | `Busy` + `CloseReason::SecurityViolation` | 与 TLS/QUIC 上层对齐 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 映射到 Tokio 的超时，保护连接建立、读写与半关闭流程。
- `CallContext::cancellation_token()` 在会话迁移或上层指令时触发，`TcpChannel` 在 `poll_ready`/IO 循环中检查并提前退出。
- `CallContext::budget` 控制单连接吞吐，结合 `BudgetDecision` 限制写入速率，确保背压信号及时反馈。
