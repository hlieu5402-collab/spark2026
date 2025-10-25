# spark-codec-rtp

## 契约映射
- 解析与生成 RTP 报文（RFC 3550），提供 `RtpPacket`、`RtpHeader` 等结构体，配合 `spark-core` 的 `BufView`/`ErasedSparkBuf` 实现零拷贝。
- `parse_rtp` 与 `RtpPacketBuilder` 与 `CallContext` 生命周期解耦，方便传输层在半关闭阶段清理未完成的帧。
- 与 SDP 协商 (`spark-codec-sdp`) 形成契约：使用 `payload_type`、`ssrc`、`sequence_number` 等字段与媒体 pipeline 同步，确保背压与重传策略可在上层实现。

## 错误分类
- 使用 `RtpError`（位于 `error` 模块）区分 Header 非法、Payload 越界、扩展长度不一致等问题，默认映射到 `ErrorCategory::ProtocolViolation`。
- 当遇到资源限制（例如 Payload 过大或缓冲不足）时返回 `ErrorCategory::ResourceExhausted`，触发 `ReadyState::BudgetExhausted`。
- 对于序列回绕判断失败这类逻辑错误，应归类为 `ErrorCategory::ImplementationError`，并记录审计日志。

## 背压语义
- 解析流程若发现缓冲尚未填满（例如分片读取），可返回 `RtpParseStatus::Incomplete`，上游将 Ready 状态保持在 `Pending`。
- 当同一 SSRC 的丢包率升高、序列号间隙过大时，宿主应借助观测指标转换为 `ReadyState::RetryAfter`，提醒上游调节码率或启用 FEC。
- 编码阶段若 `RtpPacketBuilder` 发现缓冲不足，会返回预算错误，由 `ExceptionAutoResponder` 广播 `ReadyState::BudgetExhausted`。

## TLS/QUIC 注意事项
- RTP 常运行于 DTLS-SRTP 或 QUIC DATAGRAM：
  - 本 crate 不处理加密/鉴权，但保留 header/payload 的零拷贝视图，便于 TLS/QUIC 层完成 AEAD 检查后继续解析；
  - 当底层握手失败或密钥轮换导致 CallContext 取消，解析器应立即丢弃未完成的包并遵循半关闭序列；
  - QUIC 多路复用场景中，`ssrc` 与 `CallContext` 绑定，有助于区分背压指标。

## 半关闭顺序
- FIN 触发后执行：
  1. 停止从底层拉取新包，仅处理缓冲中剩余片段；
  2. 对未完整的包返回 `Pending`/`Incomplete` 状态，由宿主决定是否等待；
  3. `closed()` 完成后丢弃所有引用，确保不会访问被释放的内存页。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| 正常收发帧 | `Ready` | 帧按照序列号顺序进入媒体管线 |
| 检测到缓冲尚未填满 | `Pending` | 等待更多分片或重传 |
| Header/扩展非法 | `Busy` | 表示对端协议违例，需记录指标 |
| Payload 超预算或缓冲不足 | `BudgetExhausted` | 触发速率限制或回退策略 |
| 丢包/拥塞导致的动态降速 | `RetryAfter` | 调用方根据观测指标发起退避 |
| 密钥/握手失败 | `Busy` + `CloseReason::SecurityViolation` | 与 TLS/QUIC 层对齐，立即半关闭 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 控制单个 RTP 会话的最长静默时间：超过阈值应触发 `close_force()` 并报告 `CloseReason::Timeout`。
- `CallContext::cancellation_token()` 在通道被上层抢占或发生错误时触发；解析循环需在每次迭代检查取消标志，避免在半关闭阶段继续消耗资源。
- `CallContext::budget(BudgetKind::Flow)` 可与速率控制器结合，在长时间高负载时返回 `BudgetDecision::Denied`，进而驱动 `ReadyState::BudgetExhausted` 告知发送端降码率。
