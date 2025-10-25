# spark-impl-tck

## 契约映射
- 提供实现层 TCK（Transport Compatibility Kit），复用 `spark-contract-tests` 的契约并结合真实传输实现（TCP/TLS/UDP）进行集成验证。
- 当前聚焦传输通道：`transport::tcp_graceful_half_close` 等测试确保 `close_graceful`、`ReadyState`、`CallContext` 在实际 IO 环境中表现一致。
- 通过 Tokio 多线程运行时模拟生产场景，确保 `CallContext` 的取消、超时与预算在真实套接字上生效。

## 错误分类
- 测试期望被测实现使用 `ErrorCategory::{Timeout, SecurityViolation, ResourceExhausted}` 等标准分类，若出现其他分类将直接 panic，提示修复。
- 对于测试框架内部错误（如运行时初始化失败），将记录 `ImplError` 并在日志中给出指引，避免误判为被测实现问题。

## 背压语义
- 用例覆盖 `ReadyState::Pending` → `Ready` 的半关闭等待、`Busy`/`RetryAfter` 的背压通知，以及 `BudgetExhausted` 的队列耗尽场景。
- 在 UDP/SIP 场景中验证 `rport` 回写与 NAT Keepalive，确保背压策略在无连接环境下仍可触发。

## TLS/QUIC 注意事项
- TLS 测试检查证书热更新、ALPN 透出、握手取消；若结合 QUIC 实现，将新增 `quic_handshake` 套件验证 0-RTT 与多路流半关闭契约。
- 所有安全相关测试会验证 ReadyState 不被污染（安全违规保持空列表）并记录 `CloseReason::SecurityViolation`。

## 半关闭顺序
- `tcp_graceful_half_close` 等测试确保：
  1. 客户端 `close_graceful` 发送 FIN；
  2. 服务端读取 EOF 后再关闭写半部；
  3. 若在 `Deadline` 内完成，则 `close_graceful` 成功；否则测试要求实现升级到 `close_force()` 并记录 `CloseReason::Timeout`。
- TCK 中的等待与断言为生产实现提供明确序列参考。

## ReadyState 映射表
| 测试 | ReadyState | 说明 |
| --- | --- | --- |
| `tcp_graceful_half_close` | `Pending` → `Ready` | 验证 FIN 等待流程 |
| `udp_rport_return` | `Ready` | 成功回写 `rport` 后继续收发 |
| `tls_handshake` | `Pending`/`Busy` | 握手等待证书或报告安全违规 |
| `tls_alpn_route` | `Ready` | 握手成功并匹配 ALPN |
| `udp_budget_exhaustion`（规划） | `BudgetExhausted` | 队列耗尽时触发 |

## 超时/取消来源（CallContext）
- 每个测试通过 `CallContext::builder()` 设置 `Deadline`、`Cancellation`，验证实现会在超时时执行 `close_force()` 并广播 `CloseReason::Timeout`。
- 测试还会取消上下文以检查 `CallContext::cancellation_token()` 的传播，防止实现忽略取消信号。
