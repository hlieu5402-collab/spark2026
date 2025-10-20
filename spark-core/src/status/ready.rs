use alloc::{borrow::Cow, fmt};
use core::task::Poll;
use core::time::Duration;

/// 服务就绪检查的核心状态枚举。
///
/// # 设计初衷（Why）
/// - **统一语义**：过往实现分别返回 `Ready/NotReady/Backpressure` 等别名，
///   使调用方难以编写跨域兼容的退避逻辑。本枚举统一抽象为四种语义：
///   - `Ready`：可立即受理请求；
///   - `Busy`：繁忙但仍保持健康；
///   - `BudgetExhausted`：调用预算（如速率、重试额度）已耗尽；
///   - `RetryAfter`：显式建议等待一段时间后重试。
/// - **跨模块复用**：路由、集群、服务 Trait 共享该结构，避免在各自模块重复声明、
///   导致术语错位。
///
/// # 结构约定（What）
/// - `Busy` 内部携带 [`BusyReason`]，用于表达繁忙原因。
/// - `BudgetExhausted` 内部携带 [`SubscriptionBudget`]，记录预算上限与剩余值。
/// - `RetryAfter` 使用 [`RetryAdvice`] 表达推荐的等待时间与可选描述。
///
/// # 逻辑提示（How）
/// - 建议调用方在收到 `Busy` 时执行指数退避或切换备份服务；
/// - 收到 `BudgetExhausted` 时应立即停止重试，待上层扩充预算后再继续；
/// - `RetryAfter` 适合作为 HTTP 429 等协议的抽象，调用方需尊重建议的时间间隔。
///
/// # 风险提示（Trade-offs）
/// - 本枚举仅承载状态，不直接包含错误信息；若需要附带错误，可使用 [`ReadyCheck`] 中的 `Err` 分支。
/// - 当实现者无额外上下文时，应优先使用 `BusyReason::QueueFull` 与 `RetryAdvice::after` 等构造函数，
///   以保持语义统一。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadyState {
    /// 服务完全就绪，可立即接收下一次请求。
    Ready,
    /// 服务处于繁忙状态，建议调用方退避或切换其他副本。
    Busy(BusyReason),
    /// 调用预算耗尽，通常表示重试次数或速率限制达到上限。
    BudgetExhausted(SubscriptionBudget),
    /// 建议等待一段时间后再重试，常用于速率限制或依赖恢复中的软性退避信号。
    RetryAfter(RetryAdvice),
}

/// 就绪检查的返回体，对 `ReadyState` 与错误进行统一包装。
///
/// # 设计思路（Why）
/// - 保持与 `Poll<Result<_, _>>` 类似的结构，但通过显式类型名表达“就绪检查”语义，
///   便于在调试与文档中指明含义。
///
/// # 契约说明（What）
/// - `Ready(ReadyState)`：就绪检查完成并返回状态；
/// - `Err(E)`：就绪检查自身失败，`E` 需实现调用方定义的错误 Trait。
///
/// # 使用准则（How）
/// - 当底层逻辑发生硬错误（例如配置损坏）时，应返回 `Err`；
/// - 若仅是暂时性拥塞，请返回 `Ready(ReadyState::Busy(_))` 或 `Ready(ReadyState::RetryAfter(_))`，
///   调用方可据此决定退避策略。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadyCheck<E> {
    /// 就绪检查成功，并给出可供调用方消费的状态。
    Ready(ReadyState),
    /// 就绪检查失败，通常需要上游中止调用或触发告警。
    Err(E),
}

/// `poll_ready` 的统一返回类型，兼容标准库中的 [`Poll`]。
///
/// # 设计背景（Why）
/// - 传统实现直接使用 `Poll<Result<(), E>>`，缺乏对“繁忙/预算耗尽”等软状态的表达力。
/// - 本类型定义为 `Poll<ReadyCheck<E>>` 的别名，并配套辅助构造函数，帮助实现者直观返回统一语义。
///
/// # 使用范式（How）
/// - `Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))`：表示“立刻可用”；
/// - `Poll::Ready(ReadyCheck::Ready(ReadyState::Busy(_)))`：表示“繁忙但可感知原因”；
/// - `Poll::Pending`：表示“仍在等待就绪”，可结合 [`RetryAdvice`] 提供退避信息；
/// - `Poll::Ready(ReadyCheck::Err(_))`：表示“检查过程中出现错误”。
///
/// # 兼容性说明（Trade-offs）
/// - 该别名保持二进制兼容性：既可在 `no_std` 场景使用，也能与现有异步运行时（Tokio、async-std）直接集成。
/// - 若后续需要扩展更多软状态，可在 [`ReadyState`] 枚举中新增分支，不影响 `PollReady` 的函数签名。
pub type PollReady<E> = Poll<ReadyCheck<E>>;

/// 服务繁忙的原因描述，帮助调用方进行针对性调度或观测。
///
/// # 设计背景（Why）
/// - 不同实现对“繁忙”有多种解释：队列堆积、依赖降速、自定义负载检测。
///   统一的枚举让调用方无需解析字符串即可理解语义。
///
/// # 契约说明（What）
/// - `QueueFull`：内部待处理队列达到或超过容量；
/// - `Custom`：自定义原因，通过 `Cow<'static, str>` 承载描述。
///
/// # 逻辑提示（How）
/// - `QueueFull` 构造函数会同时返回当前深度与容量，便于观测系统生成指标；
/// - 对于临时性原因，可使用 `Custom` 并传入短小字符串，避免额外分配。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusyReason {
    /// 队列已满，无法暂存新的任务或请求。
    QueueFull(QueueDepth),
    /// 其他自定义原因，使用 Cow 承载描述以避免不必要的分配。
    Custom(Cow<'static, str>),
}

impl BusyReason {
    /// 构造一个队列溢出的繁忙原因。
    ///
    /// # 输入参数（What）
    /// - `depth`：当前队列中的元素数量；
    /// - `capacity`：队列容量上限。
    ///
    /// # 前置条件（Contract）
    /// - `capacity` 必须大于 0；
    /// - 建议在调用前保证 `depth >= capacity`，以确保语义准确。
    ///
    /// # 后置条件（Contract）
    /// - 返回值封装为 [`BusyReason::QueueFull`]，供 `ReadyState::Busy` 使用。
    pub const fn queue_full(depth: usize, capacity: usize) -> Self {
        Self::QueueFull(QueueDepth { depth, capacity })
    }

    /// 构造一个自定义原因。
    ///
    /// # 设计考量（Trade-offs）
    /// - 使用 `Cow<'static, str>` 允许调用方在 `const` 上下文中传递字面量，
    ///   也支持在运行期分配字符串（需启用 `alloc`）。
    pub fn custom(reason: impl Into<Cow<'static, str>>) -> Self {
        Self::Custom(reason.into())
    }
}

/// 队列深度快照，用于向外暴露队列繁忙的上下文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueDepth {
    /// 队列中的元素数量。
    pub depth: usize,
    /// 队列容量上限。
    pub capacity: usize,
}

/// 预算耗尽时的上下文数据。
///
/// # 设计背景（Why）
/// - 在限流、重试预算等场景，需要同时告诉调用方“预算上限”和“剩余值”。
///   通过结构体承载该信息，可在不同实现之间保持语义一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionBudget {
    /// 预算总额度，例如允许的最大重试次数。
    pub limit: u32,
    /// 当前剩余额度。
    pub remaining: u32,
}

impl SubscriptionBudget {
    /// 通过“剩余值”判断是否耗尽，返回适配的 [`ReadyState`]。
    ///
    /// # 参数说明（What）
    /// - `limit`：预算上限；
    /// - `remaining`：当前剩余值。
    ///
    /// # 逻辑解析（How）
    /// - 当 `remaining == 0` 时返回 `ReadyState::BudgetExhausted`；
    /// - 否则返回 `ReadyState::Ready`，表示仍可继续尝试。
    ///
    /// # 风险提示（Trade-offs）
    /// - 若调用方维护的剩余值可能为负或溢出，应在调用前做范围检查。
    pub fn evaluate(limit: u32, remaining: u32) -> ReadyState {
        let budget = SubscriptionBudget { limit, remaining };
        if remaining == 0 {
            ReadyState::BudgetExhausted(budget)
        } else {
            ReadyState::Ready
        }
    }
}

/// 重试建议，用于描述“等待多久再试”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryAdvice {
    /// 推荐的等待时长。
    pub wait: Duration,
    /// 可选的原因描述，帮助调用方生成观测日志。
    pub reason: Option<Cow<'static, str>>,
}

impl RetryAdvice {
    /// 构造一个仅包含等待时间的建议。
    ///
    /// # 契约说明
    /// - `wait` 必须大于零；若无法提供准确时长，建议使用短暂的默认值（如几十毫秒）。
    pub const fn after(wait: Duration) -> Self {
        Self { wait, reason: None }
    }

    /// 为建议附加原因描述。
    pub fn with_reason(mut self, reason: impl Into<Cow<'static, str>>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

impl ReadyState {
    /// 根据队列深度生成就绪状态。
    ///
    /// # 逻辑解析（How）
    /// - 若 `depth >= capacity`，返回 `Busy(BusyReason::QueueFull)`；
    /// - 否则返回 `Ready`。
    ///
    /// # 合同说明（Contract）
    /// - 调用方需确保 `capacity > 0`，以免除以零或出现无意义的比较。
    pub fn from_queue_depth(depth: usize, capacity: usize) -> Self {
        if depth >= capacity {
            ReadyState::Busy(BusyReason::queue_full(depth, capacity))
        } else {
            ReadyState::Ready
        }
    }

    /// 根据预算剩余情况生成就绪状态。
    pub fn from_budget(limit: u32, remaining: u32) -> Self {
        SubscriptionBudget::evaluate(limit, remaining)
    }

    /// 将恢复事件映射为 `Ready` 状态，便于测试中模拟“恢复 -> 可用”的过程。
    pub const fn recovered() -> Self {
        ReadyState::Ready
    }
}

impl<E> fmt::Display for ReadyCheck<E>
where
    E: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadyCheck::Ready(state) => write!(f, "ready: {:?}", state),
            ReadyCheck::Err(err) => write!(f, "error: {}", err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：当预算剩余为 0 时，返回 `BudgetExhausted`。
    #[test]
    fn budget_exhaustion_maps_to_budget_state() {
        let status = ReadyState::from_budget(5, 0);
        assert!(matches!(
            status,
            ReadyState::BudgetExhausted(SubscriptionBudget {
                limit: 5,
                remaining: 0
            })
        ));
    }

    /// 验证：当队列溢出时，返回 `Busy`。
    #[test]
    fn queue_full_maps_to_busy() {
        let status = ReadyState::from_queue_depth(128, 64);
        assert!(matches!(
            status,
            ReadyState::Busy(BusyReason::QueueFull(QueueDepth {
                depth: 128,
                capacity: 64
            }))
        ));
    }

    /// 验证：当资源恢复后，可返回 `Ready` 状态。
    #[test]
    fn recovery_maps_to_ready() {
        let status = ReadyState::recovered();
        assert_eq!(status, ReadyState::Ready);
    }
}
