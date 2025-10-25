# spark-transport-tls

## 契约映射
- 基于 `spark-transport-tcp` 构建 TLS 1.3 通道，使用 `TlsAcceptor` 包装 `TcpChannel` 并返回 `TlsChannel`，遵循 `spark-core::transport::channel` 契约。
- `TlsChannel` 继承 `CallContext` 的取消、截止、预算逻辑，并在握手成功后更新 `SecurityContextSnapshot`，供上层记录证书、ALPN。
- `hot_reload` 模块通过 `ArcSwap` 支持证书/密钥热更新，确保运行时契约稳定。

## 错误分类
- 握手失败（证书、协议版本、ALPN）归类为 `ErrorCategory::SecurityViolation`，并在 `CloseReason` 中记录详细原因。
- 资源耗尽（线程池、会话缓存）映射到 `ErrorCategory::ResourceExhausted`，提示上游执行退避或扩容。
- 临时网络故障返回 `ErrorCategory::Retryable`，交给重试策略处理；内部 bug 归类为 `ImplError`。

## 背压语义
- 握手阶段遵循 ReadyState 契约：
  - 证书加载/CRL 检查耗时 -> `Pending`；
  - 会话缓存饱和 -> `Busy(BusyReason::queue_full)`；
  - 需要退避重试（如 OCSP 查询失败） -> `RetryAfter`。
- 数据通道继承 TCP 的背压逻辑，写缓冲耗尽时广播 `Busy`，预算不足时返回 `BudgetExhausted`。

## TLS/QUIC 注意事项
- 专注 TLS：
  - 握手参数（SNI、ALPN、证书链）通过 `SecurityContextSnapshot` 传递给上层，供 QUIC/TLS 统一审计；
  - 若未来启用 QUIC over TLS（非标准），需要复用相同的错误分类与半关闭逻辑；
  - 支持 0-RTT/会话恢复时必须校验 ReadyState，防止重放攻击污染状态。

## 半关闭顺序
- `close_graceful` 先调用 `rustls::StreamOwned::writer().close()` 发送 TLS CloseNotify，再等待对端回送确认；
- 对端未在 `Deadline` 内返回 CloseNotify 时触发 `close_force()`，记录 `CloseReason::Timeout` 并关闭底层 TCP；
- `CallContext` 取消会导致立即发送 CloseNotify 并退出，确保与 FIN 顺序一致。

## ReadyState 映射表
| TLS 阶段 | ReadyState | 说明 |
| --- | --- | --- |
| 握手成功 | `Ready` | 可开始读写应用数据 |
| 等待证书/OCSP 完成 | `Pending` | 暂停数据平面，等待外部依赖 |
| 会话缓存/线程池饱和 | `Busy(BusyReason::queue_full)` | 建议上游降载 |
| 需要退避（重试 OCSP/CRL） | `RetryAfter` | 返回建议等待时间 |
| 密码套件或证书被拒绝 | `Busy` + `CloseReason::SecurityViolation` | 立即终止连接 |
| 写入预算耗尽 | `BudgetExhausted` | 遵循 TCP 层预算控制 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 限制握手与半关闭时长，超时会触发 `TlsHandshakeError::Timeout` 并执行 `close_force()`。
- `CallContext::cancellation_token()` 用于热升级或会话迁移，取消后立即发送 CloseNotify 并关闭底层连接。
- `CallContext::budget` 控制加密开销（如记录级别压缩/解压），预算不足时向上游广播 `ReadyState::BudgetExhausted`。
