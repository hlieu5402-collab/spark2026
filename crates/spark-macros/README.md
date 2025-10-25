# spark-macros

## 契约映射
- 提供 `#[spark::service]` 过程宏，自动生成符合 `spark_core::service::Service` 契约的实现，避免业务侧手写 `poll_ready`/`call` 逻辑。
- 宏生成的 Service 会引用 `CallContext`、`ReadyState` 与 `CloseReason`，确保与 `spark-core` 的调度、半关闭语义保持一致。
- 通过公开 re-export (`spark_core::spark`) 统一入口，保证所有 crate 均以相同路径使用宏，降低版本演进的破碎风险。

## 错误分类
- 宏展开阶段的校验错误通过编译期 `syn::Error` 抛出，最终在编译日志中以 `ErrorCategory::ImplementationError` 定位（编译器错误）。
- 运行期错误仍由生成的 Service 使用 `CoreError`/`DomainError` 分类：宏保证自动引入的模板不会吞掉错误分类或改变业务语义。
- 当宏使用方式不当（如缺少重命名 `use spark_core as spark;`）会触发编译错误，编译器提示指导开发者修正。

## 背压语义
- 生成的 Service 自动实现 `poll_ready`，在 Pending 状态下记录 waker 并等待业务 Future 完成，确保背压信号通过 `ReadyState` 正确传播。
- 当业务返回 `ReadyCheck::Pending` 时，宏生成的代码会遵循 `spark-core` 的 `ReadyState` 契约，不会遗漏唤醒或误报 `Ready`。
- 宏模板保留调用方对 `CallContext::budget` 的访问能力，使自定义背压策略与预算判断得以保留。

## TLS/QUIC 注意事项
- 过程宏生成的 Service 不直接依赖 TLS/QUIC，但需确保：
  - 业务函数能接收来自 TLS/QUIC 传输层的 `CallContext`，以便在握手失败或会话迁移时响应取消；
  - 若业务逻辑涉及握手阶段的异步等待，宏生成的 `poll_ready` 会正确尊重 `CallContext::deadline()`，确保半关闭流程可被打断。

## 半关闭顺序
- 宏生成的 `call` 函数会在 Future 完成后调用 `CallContext::close_graceful()` 或上游提供的关闭接口（取决于业务逻辑）。
- 当 `close_graceful` 触发后，宏模板保证不会再次访问请求体/上下文，遵守“写半关闭 → 等待读确认 → 释放资源”的顺序。
- 若 `CallContext::deadline()` 到期，生成代码会传播 `ErrorCategory::Timeout`，驱动宿主执行 `close_force()`。

## ReadyState 映射表
| 场景 | ReadyState | 说明 |
| --- | --- | --- |
| 业务 Future 立即可用 | `Ready` | `poll_ready` 返回 `ReadyCheck::Ready(ReadyState::Ready)` |
| 业务 Future 尚未完成 | `Pending` | 存储 waker，等待完成后唤醒 |
| 业务预算耗尽 | `BudgetExhausted` | 调用方自定义逻辑通过 `CallContext::budget` 返回 |
| 依赖暂不可用 | `Busy` | 由业务在返回值中指定 `BusyReason`，宏负责透传 |
| 需要退避或重试 | `RetryAfter` | 结合重试策略返回 `ReadyState::RetryAfter` |
| TLS/QUIC 取消/安全违规 | `Busy` + `CloseReason::SecurityViolation` | `CallContext` 的关闭原因会被透传到生成代码 |

## 超时/取消来源（CallContext）
- 生成代码会在 `call` Future 内部轮询 `CallContext::cancellation_token()`，一旦被取消立即返回 `CoreError::cancelled()`。
- `CallContext::deadline()` 由宏模板传递给业务 Future，若超时则返回 `ErrorCategory::Timeout`，触发宿主的强制关闭。
- 宏不引入额外定时器，而是复用调用方提供的 `CallContext`，保证所有 crate 对超时/取消的认知一致。
