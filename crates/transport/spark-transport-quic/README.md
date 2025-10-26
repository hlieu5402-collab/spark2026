# spark-transport-quic

## 职责边界
- 基于 `quinn` 实现 QUIC 传输通道，提供多路流、0-RTT 与连接迁移能力，同时保持与 `spark-core::transport::channel` 契约一致。
- 注入 `CallContext` 的取消、截止与预算逻辑，确保每条流与连接共享统一的半关闭与背压语义。
- 输出 `SecurityContextSnapshot`、路径验证与拥塞指标，便于观测与安全审计。

## 公共接口入口
- [`src/lib.rs`](./src/lib.rs)：导出 `QuicEndpoint`, `QuicConnection`, `QuicChannel` 等核心类型，并提供运行入口。
- [`src/endpoint.rs`](./src/endpoint.rs)：管理 listener、连接握手与地址迁移。
- [`src/channel.rs`](./src/channel.rs)：实现流级别的 `poll_ready`、`read`、`write`、`close_graceful`、`close_force`。
- [`src/backpressure.rs`](./src/backpressure.rs)：根据 `ConnectionStats` 与流量控制窗口推导 `ReadyState`。
- [`src/error.rs`](./src/error.rs)：定义 `QuicError`、`HandshakeError` 等分类，并映射到 `ErrorCategory`。

## 状态机与错误域
- ReadyState 映射遵循 [`docs/state_machines.md`](../../../docs/state_machines.md)：
  - 流量控制 credit 耗尽 → `Pending`
  - 拥塞窗口收缩或排队上升 → `Busy`
  - 自适应退避（路径验证/探测丢包） → `RetryAfter`
  - 预算拒绝 → `BudgetExhausted`
- 握手失败、证书问题与 0-RTT 重放检测需映射到 `ErrorCategory::SecurityViolation`，并在 `CloseReason` 中记录原因。
- 内部 bug 或 `quinn` 错误统一映射为 `ImplementationError` 并附带调试上下文。

## 关联契约与测试
- [`crates/spark-contract-tests`](../../spark-contract-tests) 的 security、backpressure、graceful_shutdown 主题覆盖流半关闭、退避与安全事件。
- [`crates/spark-impl-tck`](../../spark-impl-tck) 的 QUIC 套件运行真实握手、连接迁移与 0-RTT 流程，校验 ReadyState 序列与错误分类。
- 观测仪表与 SLO 在 [`docs/observability/dashboards/transport-health.json`](../../../docs/observability/dashboards/transport-health.json) 中维护，确保字段同步。

## 集成注意事项
- 连接迁移需刷新 `CallContext` 中的远端地址与安全属性，防止旧上下文在新路径上生效。
- 在启用 0-RTT 时必须启用重放防御：被拒绝的 0-RTT 请求需保持 ReadyState 空列表，并仅通过 `CloseReason::SecurityViolation` 上报。
- 半关闭流程要求先调用 `channel.close_graceful()` 发送 FIN，再等待对端确认；若超时则执行 `close_force()` 并记录 `CloseReason::Timeout`。
