# spark-contract-tests

## 契约映射
- 收敛 `spark-core` 的全部公开契约，覆盖 ReadyState、预算、半关闭、错误分类等主题，通过统一的 TCK 保证实现一致性。
- 提供主题化入口函数（如 `graceful_shutdown`, `backpressure`, `errors`），供传输层与业务实现复用，以验证 `CallContext`、`CloseReason` 的一致行为。
- `Runner` 框架支持并发执行与定制化注入，使各实现 crate（例如 `spark-transport-*`）能够在 CI 中自动校验契约。

## 错误分类
- 测试会断言 `ErrorCategory::{Timeout, ResourceExhausted, SecurityViolation}` 等分类的行为，确保异常被正确映射到 ReadyState 与关闭原因。
- 若被测实现返回未知错误分类，测试将记录 `ImplError` 并失败，提醒作者更新契约。
- 所有测试失败都会附带上下文信息（ReadyState 序列、CallContext 预算等），便于快速定位分类偏差。

## 背压语义
- `backpressure` 模块验证预算消费与 `ReadyState` 的映射，包括 `BusyReason::queue_full`、`RetryAdvice`、`SubscriptionBudget` 等场景。
- `resource_exhaustion` 用例确保当队列或线程池耗尽时，自动响应器会广播正确的 ReadyState 序列。
- 传输层实现可借助这些测试确认在 `poll_ready` 中返回 `Pending` 或 `Busy` 的时机符合框架要求。

## TLS/QUIC 注意事项
- `security` 测试涵盖 TLS/QUIC 场景：0-RTT 重放、证书失效、握手超时，确保实现能够发布 `CloseReason::SecurityViolation` 并保持 ReadyState 空列表。
- QUIC 专用测试验证多路复用下的半关闭顺序与背压信号传递，防止流级别状态污染。
- 所有安全相关用例都会验证 `CallContext::cancellation_token()` 是否在握手失败时被触发。

## 半关闭顺序
- `graceful_shutdown` 套件定义标准序列：发送 FIN → 等待读写半关闭确认 → 释放资源 → 可选强制关闭。
- 测试覆盖了正常、超时、立即强制、错误路径等分支，确保实现能在 `Deadline` 到期时转为 `close_force()`。
- ReadyState 与关闭原因的断言保证半关闭期间不会提前报告成功或错误。

## ReadyState 映射表
| 测试主题 | ReadyState | 预期行为 |
| --- | --- | --- |
| `backpressure::queue_full` | `Busy(BusyReason::queue_full)` | 队列耗尽时广播拥塞信号 |
| `backpressure::budget_snapshot` | `BudgetExhausted` | 预算为零时返回快照 |
| `errors::retryable` | `RetryAfter` | 可重试错误提供退避建议 |
| `errors::resource_exhausted` | `BudgetExhausted` | 资源不足时提供快照 |
| `graceful_shutdown::wait_for_closed` | `Pending` → `Ready` | FIN 后保持 Pending，直到对端确认 |
| `security::zero_rtt_replay` | 无 ReadyState | 安全违规时保持空广播，避免状态污染 |

## 超时/取消来源（CallContext）
- 测试在构造 `CallContextBuilder` 时设置 `Deadline` 与 `Cancellation`，验证实现对超时和取消的响应。
- `graceful_shutdown::force_close_on_timeout` 用例确保 Deadline 到期会触发强制关闭并记录 `CloseReason::Timeout`。
- `context` 相关测试检查取消令牌在半关闭与错误路径上的传播，确保所有实现都遵守统一契约。
