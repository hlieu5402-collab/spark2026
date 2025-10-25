# spark-transport-udp

## 契约映射
- 封装 Tokio `UdpSocket`，为 SIP/NAT 场景提供无连接传输接口，输出 `UdpEndpoint`、`UdpReturnRoute` 等类型。
- 与 `spark-core::transport` 的地址抽象 (`TransportSocketAddr`) 对齐，支持在 `CallContext` 内记录来源、回源路径与 NAT 信息。
- `batch` 模块扩展批量发送/接收 API，为未来的 QUIC/TLS over UDP 封装提供基础设施。

## 错误分类
- 使用 `UdpError`（`thiserror::Error` 枚举）对绑定失败、IO 错误、解析失败等路径分类，映射到 `ErrorCategory::{Transport, Timeout, ResourceExhausted}`。
- 解析 `Via` 头时若发现格式错误，会返回协议类错误并提示调用方是否需要降级处理。
- 对于实现漏洞（如 NAT 路由状态不一致），错误会归类为 `ImplementationError` 并记录审计上下文。

## 背压语义
- 无连接场景下主要通过发送速率与缓冲决策体现背压：
  - 当系统缓冲达到阈值时返回 `ReadyState::Busy(BusyReason::downstream())`；
  - 若速率控制策略要求退避（例如 NAT 限制），返回 `ReadyState::RetryAfter` 并提供等待时间；
  - 当业务预算耗尽 (`CallContext::budget`) 时，直接广播 `ReadyState::BudgetExhausted`。
- `recv_from` 在未读到完整报文时保持 `Pending`，确保上层不会误判通道可用。

## TLS/QUIC 注意事项
- UDP 是 TLS/QUIC 的底座：
  - 需要在返回的 `UdpReturnRoute` 中保留源地址/端口，供 QUIC 握手验证地址一致性；
  - 对 DTLS/QUIC 等安全协议，`CallContext` 的取消与超时应在握手阶段传递给上层，以便及时执行 `close_force`；
  - 若检测到重放或非法来源，应返回 `ErrorCategory::SecurityViolation` 并停止发送。

## 半关闭顺序
- UDP 无连接但仍需遵循半关闭契约：
  1. 当上层决定停止发送时，先通知业务层并停止调用 `send_to`；
  2. 等待 `CallContext::closed()` 以确认上层不再期待响应；
  3. 超时则调用 `close_force()`，释放 socket 并记录 `CloseReason::Timeout`。
- 虽无 FIN 报文，但需确保资源释放顺序与 TCP/TLS/QUIC 一致。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| 正常可写 | `Ready` | 可以继续发送数据 |
| 系统缓冲暂满 | `Pending` | 等待下一次 `poll_write_ready` |
| 持续拥塞/速率限制 | `Busy(BusyReason::downstream)` | 提醒上游降低速率 |
| 需要退避（NAT/节流） | `RetryAfter` | 返回等待时间，避免触发防护机制 |
| 预算耗尽 | `BudgetExhausted` | `CallContext::budget` 拒绝发送 |
| 安全事件（非法地址） | `Busy` + `CloseReason::SecurityViolation` | 停止交互并报警 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 控制 UDP 会话或心跳的最长等待，过期后执行 `close_force()` 并记录 `CloseReason::Timeout`。
- `CallContext::cancellation_token()` 在业务决定迁移/下线时触发，`UdpEndpoint` 应立即停止接收并清理 NAT 状态。
- `CallContext::budget` 可用于限制每个终端的带宽或请求数，与 ReadyState 映射协同防止滥用。
