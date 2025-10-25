# spark-contract-tests-macros

## 契约映射
- 提供 `#[spark_tck]` 属性宏，为模块自动注入契约测试入口，确保各 crate 在 CI 中执行 `spark-contract-tests` 的标准套件。
- 宏根据 `suites(...)` 参数选择主题（背压、取消、错误、状态机等），并生成对应的 `run_*_suite` 调用，与 `spark-core` 契约保持一致。
- 默认套件覆盖 ReadyState、CallContext、半关闭等核心行为，调用方无需手工维护测试入口即可对齐契约。

## 错误分类
- 宏展开失败时返回编译错误，提醒使用者检查属性参数；这类错误被视为 `ErrorCategory::ImplementationError`，在 CI 阶段即被发现。
- 运行时若某套件失败，测试框架会保留原始错误分类（如 `ResourceExhausted`），宏不会重写分类，确保诊断信息准确。

## 背压语义
- 通过注入标准套件，间接验证背压行为（ReadyState Busy/RetryAfter/预算决策）。宏保证所有被测实现都执行这些断言，维持背压契约一致。
- 当新增背压相关套件时，只需在 `default_suite_idents` 中登记，即可覆盖所有使用宏的模块。

## TLS/QUIC 注意事项
- `spark_tck` 套件包含 TLS/QUIC 用例（如 0-RTT 重放、握手超时）；宏自动拉起这些测试，确保实现不会遗漏安全场景。
- 调用方可通过 `suites(quic_security)` 等方式显式开启扩展主题，验证 QUIC 特定的 ReadyState/半关闭行为。

## 半关闭顺序
- `graceful_shutdown` 套件默认纳入测试，宏在生成的测试函数中调用 `run_graceful_shutdown_suite()`，验证“写半关闭 → 等待确认 → 释放资源”序列。
- 若实现需要额外的半关闭变体，可通过宏参数追加自定义套件，确保测试覆盖完整。

## ReadyState 映射表
| 套件 | ReadyState | 说明 |
| --- | --- | --- |
| `backpressure` | `Busy`/`RetryAfter`/`BudgetExhausted` | 验证背压映射 |
| `errors` | `RetryAfter`/`BudgetExhausted`/静默 | 确保错误分类触发正确信号 |
| `cancellation` | `Pending` → 取消 | 验证取消令牌传播 |
| `state_machine` | 各 ReadyState | 确认状态机转换矩阵 |
| `observability` | `Ready` | 确保观测链路不引入额外背压 |
| `hot_swap` | `Pending` | 验证滚动升级期间的等待语义 |

## 超时/取消来源（CallContext）
- 所有被宏注入的套件都会构造带 `Deadline` 与 `Cancellation` 的 `CallContext`，验证实现能正确处理超时与取消。
- 若实现忽略超时或取消，相关测试会失败并输出 ReadyState/CloseReason 序列，帮助定位问题。
