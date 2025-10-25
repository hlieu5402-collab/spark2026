# spark-codec-line

## 契约映射
- 实现 `spark_core::codec::Codec` trait，将行分隔文本映射到 `CallContext` 上下文，使扩展 crate 也能复用核心缓冲池与预算契约。
- `EncodeContext`/`DecodeContext` 的使用与 `spark-core` 完全一致：编码时租借缓冲、校验 `max_frame_size`；解码时消费 `ErasedSparkBuf` 并返回 `DecodeOutcome`。
- 通过 `CodecDescriptor` 注册至 `CodecRegistry`，与传输层共享同一 Ready/半关闭契约，无需额外桥接层。

## 错误分类
- 编解码失败使用 `CoreError::new` 并携带 `error::codes::PROTOCOL_*` 系列编码：预算超限映射为 `protocol.budget_exceeded`，UTF-8 解析失败映射为 `protocol.decode`。
- 按契约区分“实现错误”与“业务错误”：本 crate 仅产生协议类错误，不会直接抛出 `DomainError`。
- 所有错误均通过 `ExceptionAutoResponder` 自动翻译为 ReadyState 与关闭原因，调用方可在日志/指标中按编码聚合。

## 背压语义
- 编码阶段若预算不足会立即返回 `CoreError`，触发 `ReadyState::BudgetExhausted`。
- 解码阶段返回 `DecodeOutcome::Incomplete` 时不触发错误，由传输栈维持 `ReadyState::Pending`，防止误判为拥塞。
- 当检测到换行但帧超限时，返回 `CoreError`，上游会广播 `ReadyState::Busy` 或 `RetryAfter`（取决于调用方重试策略）。

## TLS/QUIC 注意事项
- 本 crate 不直接处理 TLS/QUIC，但遵守以下约束：
  - 所有缓冲操作使用 `ErasedSparkBuf`，确保可在 TLS/QUIC 零拷贝通道上运行。
  - 若 TLS/QUIC 握手失败导致 `CallContext` 取消，编码与解码接口需立即返回 `CoreError::cancelled()`（由宿主注入），避免阻塞半关闭流程。

## 半关闭顺序
- 作为纯编解码器，本 crate 不主动执行半关闭，但必须保证：
  - 在 `close_graceful` 触发后，所有待处理的 `DecodeOutcome::Incomplete` 均可被丢弃，不会再次访问已释放缓冲。
  - 编码端收到 FIN 后应停止租借缓冲，避免破坏“先写半关闭、再等待读确认”的顺序。

## ReadyState 映射表
| 编解码阶段 | ReadyState | 说明 |
| --- | --- | --- |
| `encode` 成功 | `Ready` | 不额外产生背压信号，由上游继续发送 |
| `encode` 预算不足 | `BudgetExhausted` | `CoreError` 使用 `protocol.budget_exceeded`，被自动转换 |
| `decode` 返回 `DecodeOutcome::Incomplete` | `Pending` | 由宿主在 `poll_ready` 中保持等待状态 |
| `decode` 成功解析一行 | `Ready` | 立即唤醒后续处理 |
| `decode` 检测到格式错误 | `Busy` | 标记为下游可重试的协议违例，通常伴随观察指标 |
| TLS/QUIC 取消或 CallContext 超时 | `RetryAfter` | 宿主依据 `Deadline` 推导退避窗口，编解码器返回取消错误 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 决定编码/解码最长等待：
  - 解码需在 `DecodeContext` 暴露的计时器内返回 `Incomplete`，让宿主在超时时转为 `close_force`。
  - 编码应在检测到取消 (`ctx.cancellation_token().is_cancelled()`) 时提前返回，避免在半关闭阶段继续写入。
- 取消信号统一由宿主注入，编解码器不自建定时器，只负责传播 `CoreError::cancelled()`，确保与传输栈的错误分类一致。
