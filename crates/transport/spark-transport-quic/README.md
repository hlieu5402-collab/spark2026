# spark-transport-quic

## 契约映射
- 基于 `quinn` 提供 QUIC 传输实现，暴露 `QuicEndpoint`、`QuicConnection`、`QuicChannel`，与 `spark-core::transport::channel` API 等价。
- `run_with_context` 将 `CallContext` 的取消、截止、预算注入到 `quinn` 的事件循环中，确保多路复用流遵守统一契约。
- `QuicBackpressure` 根据 `ConnectionStats` 映射背压信号，与 TCP/TLS 保持一致，使 ReadyState 在不同传输类型间可比较。

## 错误分类
- 握手失败（TLS、证书、版本不匹配）归类为 `ErrorCategory::SecurityViolation`，并通过 `CloseReason` 报告具体原因。
- 流量控制/拥塞导致的数据写入失败映射到 `ErrorCategory::ResourceExhausted` 或 `Retryable`，提示调用方采取退避策略。
- 实现缺陷或 quinn 内部错误转译为 `ImplError`，同时记录调试上下文。

## 背压语义
- `QuicBackpressure` 关注三类指标：
  - 发送字节积压 → `Busy(BusyReason::downstream)`；
  - 拥塞窗口缩减 → `RetryAfter`，提供重试节奏；
  - 流量控制 credit 耗尽 → `BudgetExhausted`。
- `QuicChannel::poll_ready` 会在 Pending 时保存 waker，并在流可写后恢复 `Ready`，避免多路复用流饿死。

## TLS/QUIC 注意事项
- QUIC 自带 TLS 1.3：
  - 握手信息（ALPN、证书指纹、0-RTT 状态）通过 `SecurityContextSnapshot` 暴露给上层；
  - 0-RTT 重放测试确保 ReadyState 不被污染，安全事件应保持 ReadyState 空列表并报告 `SecurityViolation`；
  - 支持密钥更新与连接迁移时必须刷新 `CallContext`，避免旧上下文在新路径上生效。

## 半关闭顺序
- QUIC 流半关闭遵循：
  1. `QuicChannel::shutdown(Write)` 发送 FIN 帧；
  2. 等待对端确认写半关闭，并继续读取直到读半关闭完成；
  3. 若 `Deadline` 到期仍未完成，调用 `close_force()` 终止流并记录 `CloseReason::Timeout`。
- `spark-core::transport::ShutdownDirection` 确保调用方显式指定方向，防止误关闭整个连接，并在所有传输实现之间提供一致语义。

## ReadyState 映射表
| 流状态 | ReadyState | 说明 |
| --- | --- | --- |
| 流可写且拥塞正常 | `Ready` | 可继续发送帧 |
| 流量控制 credit 耗尽 | `Pending` | 等待对端更新窗口 |
| 拥塞窗口收缩/排队增长 | `Busy(BusyReason::downstream)` | 提醒调度器减速 |
| 需要退避（探测丢包/路径验证） | `RetryAfter` | 携带退避建议 |
| Budget 或 connection 限制 | `BudgetExhausted` | 由 `QuicBackpressure` 判定 |
| 握手/密钥失败 | `Busy` + `CloseReason::SecurityViolation` | 立即终止流 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 控制握手、流建立与半关闭时长，超时触发 `close_force()` 并报告 `CloseReason::Timeout`。
- `CallContext::cancellation_token()` 支持连接迁移或上游中断，取消后立即停止打开新流并关闭现有流。
- `CallContext::budget` 可绑定到 QUIC 的流量控制窗口，超限时通过 `BudgetDecision` 阻止进一步发送并广播 `ReadyState::BudgetExhausted`。
