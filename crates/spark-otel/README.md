# spark-otel

## 契约映射
- 将 `spark-core` 的可观测性契约与 OpenTelemetry 对齐：`install` 注册全局 TracerProvider，并将 `HandlerSpanTracer` 映射到 OTel Span。
- `TraceContext`/`TraceState` 的互转遵循 W3C Trace Context 规范，保持 `CallContext` 中的 `trace_flags`、`trace_state` 与外部系统一致。
- 通过 `pipeline::instrument` 将 Handler 级别事件同步到 OTel，确保 ReadyState、错误分类、背压信号都能被观测链路记录。

## 错误分类
- 安装过程中的错误由 `spark_otel::Error` 表示：重复安装、已有 Subscriber、TraceState 转换失败、HandlerTracer 已安装等。
- 运行期异常（如上报失败）由 `HandlerTracerError` 与 `CoreError` 结合，最终映射到 `ErrorCategory::ObservabilityFailure` 或 `ImplementationError`。
- 在 OTel 管道内传播的业务错误保持原始分类，`spark-otel` 只负责附加属性，不会重新包装。

## 背压语义
- 可观测性链路不应引入背压：`HandlerSpanGuard` 在 Drop 时异步发送 Span，若 OTel 导出器阻塞，将记录 `ReadyState::Busy` 诊断信息而不阻塞主流程。
- 当导出器内存队列达到上限时，`spark-otel` 会触发 `CallContext::budget(BudgetKind::Observability)`，并在指标中记录丢弃数量，提醒上游扩容。
- 对于延迟采样策略，可基于 ReadyState 信号（Busy/BudgetExhausted）动态调整采样率，防止观测系统成为背压源。

## TLS/QUIC 注意事项
- TraceContext 中会记录 TLS/QUIC 握手信息（ALPN、加密套件）作为 Span Attributes，便于排查握手失败。
- 当 TLS/QUIC 握手失败并触发 `CloseReason::SecurityViolation`，`spark-otel` 会在 Span 上记录事件并设置错误状态，帮助安全团队追踪。
- 对 QUIC 0-RTT 场景，TraceState 转换会保留 0-RTT 指示位，防止 ReadyState 重放测试遗漏上下文。

## 半关闭顺序
- `HandlerSpanGuard` 在 FIN 阶段会先记录 `close_graceful` 事件，再等待 `closed()` 完成后关闭 Span，遵循“写 FIN → 等待确认 → 释放”。
- 若 `Deadline` 触发导致 `close_force()`，Span 会记录 `timeout` 事件并附带 `ErrorCategory::Timeout`，与 `CallContext` 契约对齐。
- 确保在强制关闭后不再访问 `CallContext` 内的可变引用，避免破坏安全性审计链路。

## ReadyState 映射表
| 观测场景 | ReadyState | 说明 |
| --- | --- | --- |
| 正常安装与导出 | `Ready` | 可继续生成 Span |
| 导出器暂时阻塞 | `Busy` | 记录 `BusyReason::custom("otel.exporter_blocked")` |
| 观测预算耗尽 | `BudgetExhausted` | 由 `BudgetKind::Observability` 控制，触发丢弃策略 |
| 需要退避采样 | `RetryAfter` | 根据导出器反馈设置退避窗口 |
| 等待外部系统注册（如 Collector） | `Pending` | 安装流程保持等待，不阻塞业务流 |
| TLS/QUIC 安全事件 | `Busy` + `CloseReason::SecurityViolation` | 在 Span 中记录安全告警 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 影响 Handler Span 的生命周期：当超过 Deadline，Span 自动标记为超时并触发 `close_force()`。
- `CallContext::cancellation_token()` 确保在会话被取消时立即结束相关 Span，防止观测数据悬挂。
- `CallContext::budget`（Observability）帮助控制指标/日志量，超限时会回退到采样模式并报告 `ReadyState::BudgetExhausted`。
