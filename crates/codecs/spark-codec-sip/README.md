# spark-codec-sip

## 契约映射
- 负责将 SIP 文本报文映射为结构化对象（`RequestLine`、`Header` 等），供 `spark-core` 的 `CodecRegistry` 与上层业务使用。解析与序列化过程遵循 RFC 3261，并保持零拷贝引用以贴合核心缓冲契约。
- `SipCodecScaffold` 为 `spark-impl-tck`、`spark-core` 的动态扩展提供稳定入口，后续可封装为 `TypedCodecFactory` 与传输层协作。
- 解析函数 `parse_request`/`parse_response` 与 `fmt` 模块联合保证 CallContext 的生命周期要求：输入缓冲在半关闭结束前始终可用，符合 `ErasedSparkBuf` 语义。

## 错误分类
- 解析错误统一使用 `SipParseError`，覆盖请求行、头部、URI 等语法分支；序列化错误通过 `SipFormatError` 区分 IO 与编码失败。
- 在 `spark-core` 环境中，这些错误将被转换为 `CoreError`，通常映射到 `ErrorCategory::ProtocolViolation`。`UnexpectedEof` 场景会触发 `ReadyState::Busy`，提醒上游检查网络分段。
- 不直接定义业务错误分类，业务层若需自定义，应在 `DomainError` 中补充并传递给观察面。

## 背压语义
- 当解析遇到 `UnexpectedEof` 或 `InvalidHeaderValue` 这类需要额外数据的场景，推荐调用方将 Ready 状态设置为 `Pending`，等待更多字节后再次解析。
- 对于格式错误导致的 `SipParseError::*`，框架会广播 `ReadyState::Busy`（表示依赖/数据异常）或 `RetryAfter`（若结合重试策略）。
- 若 header 过多或体积异常，宿主可结合 `CallContext::budget(BudgetKind::Flow)` 转译为 `ReadyState::BudgetExhausted`，本 crate 的 API 会传播该错误编码。

## TLS/QUIC 注意事项
- SIP 文本通常承载在 TLS 或 QUIC 信令通道上，本 crate 遵循：
  - 不缓存底层缓冲所有权，允许 TLS/QUIC 层在半关闭阶段安全释放内存；
  - 遇到握手失败或证书校验错误时，宿主会注入取消信号，解析流程需立即停止并向上传播 `ErrorCategory::SecurityViolation`。
- QUIC 多路复用下，调用方应使用 `CallContext::connection_id()`（若实现）区分流；本 crate 的解析函数不共享跨流状态，避免重放污染。

## 半关闭顺序
- 当 `CallContext::close_graceful()` 启动 FIN：
  1. 传输栈停止向解析器递交新缓冲，本 crate 只消费已收到的数据。
  2. 若仍存在半帧（header 未完成），解析器返回 `SipParseError::UnexpectedEof`，由宿主选择重试或终止。
  3. 在 `closed()` future 完成后不再访问底层切片，确保与 `graceful-shutdown-contract` 对齐。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| 解析成功/写入成功 | `Ready` | 可继续处理下一条 SIP 消息 |
| 解析遇到半帧（`UnexpectedEof`） | `Pending` | 等待传输层补齐，保持上下文存活 |
| 语法错误（`InvalidHeader*` 等） | `Busy` | 表示输入异常，提醒上游排查依赖或数据源 |
| Header/Body 超预算 | `BudgetExhausted` | 通过 `CallContext` 预算判断，阻断超大报文 |
| 网络抖动需退避（结合重试策略） | `RetryAfter` | 宿主根据错误分类转换，避免立即重放 |
| 握手/证书错误 | `Busy` + `CloseReason::SecurityViolation` | 与 TLS/QUIC 层保持一致 |

## 超时/取消来源（CallContext）
- `CallContext::deadline()` 控制解析最长等待；若在 Deadline 内未收到完整报文，宿主触发 `close_force()` 并记录 `CloseReason::Timeout`。
- `CallContext::cancellation_token()` 可在会话被其他组件抢占时立即终止解析；本 crate 需在外围循环中检测取消标志，并停止调用解析函数。
- 通过 `CallContext::budget()` 与 `limits::BudgetKind::Flow` 结合，可限制单路 SIP 消息的解析成本，避免 DoS。超限后返回的错误会自动落入 `ErrorCategory::ResourceExhausted`。
