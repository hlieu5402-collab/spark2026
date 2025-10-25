# spark-codec-sdp

## 契约映射
- 负责 Session Description Protocol (SDP) 的解析与生成，承载 SIP 信令和 RTP/RTCP 数据面的能力协商。`SessionDesc`、`MediaDesc` 等结构遵循 `spark-core` 的零拷贝契约，与 `CallContext` 生命周期保持一致。
- 解析函数 `parse_sdp` 与 `format_sdp` 将文本行映射为结构化数据，方便后续传输层根据媒体参数调整背压策略（带宽、码率、ICE 候选等）。
- 通过 `TypedCodecFactory`（规划中）可直接注册至 `CodecRegistry`，使得 SDP 套件也能遵循统一的半关闭与 ReadyState 语义。

## 错误分类
- 使用 `SdpParseError`/`SdpFormatError`（位于 `error` 模块）标识语法或格式失败，后续由 `spark-core` 转译为 `ErrorCategory::ProtocolViolation`。
- 对于媒体行缺失或属性非法的场景，推荐调用方将错误映射为 `ReadyState::Busy` 并记录 `CloseReason::ProtocolViolation`。
- 若 body 超出协商范围（例如码率超过预算），可以结合 `CallContext::budget` 返回 `ErrorCategory::ResourceExhausted`，触发自动背压。

## 背压语义
- 当 `parse_sdp` 遇到不完整的媒体块（例如 ICE 属性尚未到齐），可返回半结构并提示上游保持 `Pending` 状态，等待完整协商信息。
- 对于解析成功但资源需求超过预算的 SDP（如多路视频流），应由上游根据 `MediaDesc` 中的带宽、码率将 `ReadyState` 置为 `RetryAfter` 或 `BudgetExhausted`。
- 编码阶段若输出缓冲不足，会使用 `CoreError::new` 返回预算错误，从而触发 `ReadyState::BudgetExhausted`。

## TLS/QUIC 注意事项
- SDP 可携带 `a=fingerprint`、`a=setup` 等 DTLS/QUIC 参数：
  - 本 crate 不直接校验证书，但会保留原始字段供 `spark-transport-tls`/`quic` 检查；
  - 若握手协商失败，宿主会通过 `CallContext` 取消流程，解析函数需立即退出并传播 `ErrorCategory::SecurityViolation`。
- QUIC 下的多流场景需确保 SDP 协商完成后再打开数据流，本 crate 的结构体提供 `media` 列表供调度器判定背压策略。

## 半关闭顺序
- 在 `close_graceful` 期间：
  1. 停止解析新的 SDP 消息，仅处理已收到的文本块；
  2. 若媒体描述尚未完成，则返回部分结果并提示 `Pending`，让宿主决定是否等待；
  3. 当 `closed()` 完成后不再引用底层缓冲，避免破坏 FIN → 等待读确认 → 释放的顺序。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| SDP 解析/生成成功 | `Ready` | 可继续协商后续媒体参数 |
| ICE/媒体属性缺失导致协商暂挂 | `Pending` | 等候对端补齐，保持 CallContext 存活 |
| SDP 中声明的资源需求超限 | `BudgetExhausted` | 依据 `BudgetKind::Flow`/`Bandwidth` 的决策 |
| 解析遇到语法错误 | `Busy` | 报告依赖异常，触发观测告警 |
| 需要退避重新谈判（如重邀请） | `RetryAfter` | 结合重试策略给出退避窗口 |
| 握手安全参数无效 | `Busy` + `CloseReason::SecurityViolation` | 终止协商并通知安全系统 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 用于限制 SDP 协商时间：超时后必须调用 `close_force()` 并打断等待中的 `Pending` 状态。
- `CallContext::cancellation_token()` 在媒体重协商或上层释放资源时触发；一旦收到取消信号，应停止解析新的 SDP 内容。
- 预算 (`CallContext::budget`) 可结合媒体带宽、并发流数量，提前阻止超配。若预算被拒绝，调用方应广播 `ReadyState::BudgetExhausted` 并回收资源。
