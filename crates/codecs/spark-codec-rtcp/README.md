# spark-codec-rtcp

## 契约映射
- 实现 RTCP 控制报文的解析与构造，输出 `RtcpPacket`、`SenderReport`、`ReceiverReport` 等结构，供媒体质量监控与拥塞控制模块消费。
- 与 `spark-codec-rtp` 协同：解析出的统计字段（丢包率、抖动、NTP 时间）映射到上层 `CallContext`，帮助决策重试与背压策略。
- 通过 `RtcpCodecScaffold` 向 `spark-impl-tck` 与未来的 `CodecRegistry` 暴露一致入口，使得测试与生产实现共享契约。

## 错误分类
- 使用 `RtcpError` 列举 Packet Header、长度、复合包边界等错误。协议违例映射到 `ErrorCategory::ProtocolViolation`，并触发 `ReadyState::Busy`。
- 对于占用超过预算（例如复合包过大）的场景，返回 `ErrorCategory::ResourceExhausted`，促使宿主广播 `ReadyState::BudgetExhausted`。
- 当检测到实现 bug（如内部状态不一致）时，错误会被标记为 `ErrorCategory::ImplementationError`，并要求在审计日志中记录。

## 背压语义
- 解析复合包时如果发现缓冲不足，可提前返回 `RtcpError::Truncated`，上游应保持 `ReadyState::Pending` 等待更多数据。
- 若 Sender Report 展示持续高丢包或 RTT 激增，可将 ReadyState 切换为 `RetryAfter`，提醒调用方调整发送节奏或码率。
- 发送 SDES/BYE 时若遇到缓冲限制，`RtcpPacketBuilder`（计划中）将返回预算错误，驱动 `BudgetExhausted` 信号。

## TLS/QUIC 注意事项
- RTCP 常与 RTP 共用 DTLS-SRTP 或 QUIC 连接：
  - 本 crate 保持零拷贝视图，允许安全层在半关闭阶段回收密钥；
  - 握手失败或重放检测触发时，宿主通过 `CallContext` 取消流程，解析器需立即停止工作；
  - QUIC DATAGRAM 模式下，每个 RTCP 复合包应绑定到独立的流 ID/会话索引，防止状态混淆。

## 半关闭顺序
- 当通道执行优雅关闭：
  1. 传输层先发送 BYE/空包完结统计，再调用 `close_graceful()`；
  2. 本 crate 仅消费已收到的复合包，未完成的包返回 `Pending`，等待 `closed()` 确认；
  3. 超时 (`Deadline`) 触发时，宿主会强制结束并报告 `CloseReason::Timeout`，本 crate 不再访问底层缓冲。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| 正常解析 SR/RR/SDES | `Ready` | 可继续推送统计数据 |
| 复合包截断或待补齐 | `Pending` | 等待传输层补齐剩余字节 |
| Header/长度违例 | `Busy` | 表示对端协议实现错误，需要记录告警 |
| 复合包尺寸超预算 | `BudgetExhausted` | 防止被异常客户端拖垮 |
| 统计指标提示退避（高 RTT/丢包） | `RetryAfter` | 上游依据统计做节流 |
| 握手/鉴权失败 | `Busy` + `CloseReason::SecurityViolation` | 触发安全流程并关闭通道 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 限制控制面响应时间，避免在 FIN 后仍等待远端统计包；超时后由宿主调用 `close_force()`。
- `CallContext::cancellation_token()` 在媒体会话被结束或迁移时触发；解析循环需在每个复合包后检查取消标志。
- 预算接口 (`CallContext::budget`) 可对单次复合包的内存消耗设限，防止异常设备发送超大 SDES/BYE 消息导致内存峰值。
