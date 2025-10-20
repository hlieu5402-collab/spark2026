//! 服务就绪与背压语义的权威锚点模块。
//!
//! ## 设计目标（Why）
//! - **统一语义出口**：集中定义 `ReadyState`、`ReadyCheck` 与 [`PollReady`]，避免在路由、服务 Trait
//!   等子域重复声明枚举导致语义漂移。
//! - **支撑统一文档**：通过 `cargo doc` 仅生成此处的类型说明，让调用方在查阅 API 时不会遇到多个冲突
//!   定义，降低认知成本。
//!
//! ## 契约说明（What）
//! - 所有对外暴露的就绪检查函数必须返回 [`PollReady`]，错误类型由调用场景自定义；
//! - 唯一合法的别名声明形如 `type PollReady = Poll<ReadyCheck<_>>`，不得额外定义平行枚举；
//! - 模块同时提供 [`BusyReason`]、[`RetryAdvice`] 等上下文结构，作为上层实现的组成部分。
//!
//! ## 集成指引（How）
//! - 上层若需要扩展状态，须在 [`ReadyState`] 中新增分支，并更新相关构造函数或文档；
//! - 若现有代码存在自定义 `PollReady` 枚举，必须迁移至 `type PollReady<E> = Poll<ReadyCheck<E>>` 的别名，
//!   以便框架内的泛型与对象层均能共享一致的签名；
//! - 扩展文档或教程时，请引用 `spark-core::status::ready` 作为唯一的 API 链接，确保“单一事实来源”。
//!
//! ## 风险与注意事项（Trade-offs）
//! - 在 `no_std` 环境下依赖 `alloc`，需要调用方在启用 `alloc` 特性时同步链接；
//! - 扩展状态时须评估对序列化、兼容层的影响，避免新分支破坏旧版本调用方的匹配逻辑。
use crate::contract::{BudgetDecision, BudgetSnapshot};
use alloc::{borrow::Cow, fmt};
use core::convert::TryFrom;
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
#[rustfmt::skip]
pub/* 状态锚点别名 */ type PollReady<E> = Poll<ReadyCheck<E>>;

/// 服务繁忙的原因描述，帮助调用方进行针对性调度或观测。
///
/// # 设计背景（Why）
/// - 不同实现对“繁忙”有多种解释：队列堆积、依赖降速、自定义负载检测。
///   统一的枚举让调用方无需解析字符串即可理解语义。
///
/// # 契约说明（What）
/// - `Upstream`：上游链路（如路由、业务线程池）过载；
/// - `Downstream`：下游依赖或对端主动施加背压；
/// - `QueueFull`：内部待处理队列达到或超过容量；
/// - `Custom`：自定义原因，通过 `Cow<'static, str>` 承载描述。
///
/// # 逻辑提示（How）
/// - `QueueFull` 构造函数会同时返回当前深度与容量，便于观测系统生成指标；
/// - 对于临时性原因，可使用 `Custom` 并传入短小字符串，避免额外分配。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BusyReason {
    /// 上游链路繁忙，例如路由层、业务线程池或 RPC 客户端暂时过载。
    Upstream,
    /// 下游链路繁忙，例如目标服务返回 `429` 或传输通道窗口耗尽。
    Downstream,
    /// 队列已满，无法暂存新的任务或请求。
    QueueFull(QueueDepth),
    /// 其他自定义原因，使用 Cow 承载描述以避免不必要的分配。
    Custom(Cow<'static, str>),
}

impl BusyReason {
    /// 构造一个上游过载的繁忙原因。
    ///
    /// # 教案式说明
    /// - **意图 (Why)**：在路由、聚合等调用链条中，最常见的繁忙信号来自上游排队或线程池耗尽；
    ///   该函数提供显式构造器，降低调用方误用 `Custom("upstream")` 等魔法字符串的概率。
    /// - **契约 (What)**：
    ///   - **输入**：无额外参数，表示语义固定；
    ///   - **输出**：[`BusyReason::Upstream`] 常量；
    ///   - **前置条件**：调用点需确认繁忙确实来自上游组件；
    ///   - **后置条件**：返回值可直接嵌入 [`ReadyState::Busy`]，供指标或退避策略使用。
    /// - **风险提示 (Trade-offs & Gotchas)**：若未来需要携带更丰富的上游上下文，应优先扩展
    ///   [`BusyReason`] 枚举而非在调用点拼接字符串，以保持统一抽象。
    pub const fn upstream() -> Self {
        Self::Upstream
    }

    /// 构造一个下游施加背压的繁忙原因。
    ///
    /// # 教案式说明
    /// - **意图 (Why)**：封装常见的“目标服务过载/窗口耗尽”等场景，方便上层根据来源切换备份或降级策略；
    /// - **契约 (What)**：
    ///   - **输入**：无；
    ///   - **输出**：[`BusyReason::Downstream`] 常量；
    ///   - **前置条件**：调用方需确认繁忙由下游引起；
    ///   - **后置条件**：可直接用于构建 [`ReadyState::Busy`] 或透传至观测系统。
    /// - **风险提示 (Trade-offs & Gotchas)**：与 `upstream` 类似，若未来需要区分不同下游角色，可在枚举上新增
    ///   结构化字段，而非退回字符串描述。
    pub const fn downstream() -> Self {
        Self::Downstream
    }

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

impl From<&BudgetSnapshot> for SubscriptionBudget {
    /// 将契约层的 `BudgetSnapshot` 映射为订阅预算结构。
    ///
    /// # 教案式说明
    /// - **意图 (Why)**：背压模块在处理预算耗尽时持有 [`BudgetSnapshot`]（`u64` 精度），
    ///   而就绪检查需要以 [`SubscriptionBudget`] 对外呈现统一接口；因此实现 `From`
    ///   以便在 `poll_ready` 流程中无缝转换。
    /// - **契约 (What)**：
    ///   - **输入**：引用类型的 `BudgetSnapshot`，保证零拷贝；
    ///   - **输出**：新的 [`SubscriptionBudget`] 实例，`limit` 与 `remaining` 会以饱和方式
    ///     截断到 `u32` 范围；
    ///   - **前置条件**：调用方需确保快照与当前就绪检查上下文匹配；
    ///   - **后置条件**：返回结构可直接嵌入 `ReadyState::BudgetExhausted`。
    /// - **实现 (How)**：逐个字段取值，使用 [`u32::try_from`] 搭配 `unwrap_or(u32::MAX)`
    ///   完成饱和截断，避免溢出 panic。
    /// - **风险提示 (Trade-offs & Gotchas)**：当原始值超过 `u32::MAX` 时，转换结果会被
    ///   截断为 `u32::MAX`，调用方若需精确值应直接传递 `BudgetSnapshot` 供日志或指标使用。
    fn from(snapshot: &BudgetSnapshot) -> Self {
        let limit = u32::try_from(snapshot.limit()).unwrap_or(u32::MAX);
        let remaining = u32::try_from(snapshot.remaining()).unwrap_or(u32::MAX);
        SubscriptionBudget { limit, remaining }
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

    /// 根据契约层的预算决策生成统一的就绪状态。
    ///
    /// # 教案级说明：`from_budget_decision`
    ///
    /// ## 意图与背景（Why）
    /// - 历史上在兼容层中常见“Busy 与 Budget 混搭”这种写法，导致调用方无法通过判等区分“临时繁忙”与“预算耗尽”。
    /// - 该方法作为唯一入口，将 [`BudgetDecision`] 明确映射到 [`ReadyState::BudgetExhausted`] 或 `Ready`，统一预算语义。
    ///
    /// ## 架构定位（Where）
    /// - 处于 `status::ready` 核心模块，供 `compat_v0` 及后续迁移逻辑调用；
    /// - 与 [`SubscriptionBudget`] 等工具函数配合，构成“契约判定 → 就绪信号”的标准路径。
    ///
    /// ## 契约定义（What）
    /// - **输入**：`decision` 必须来源于 [`Budget::try_consume`](crate::contract::Budget::try_consume)
    ///   或等价逻辑，保证快照与调用上下文一致；
    /// - **输出**：当预算剩余为 0 时返回 `ReadyState::BudgetExhausted`，否则返回 `Ready`；
    /// - **前置条件**：调用方需确保决策对应当前请求，避免不同预算之间的交叉污染；
    /// - **后置条件**：本函数不改变预算数值，仅负责语义映射。
    ///
    /// ## 解析逻辑（How）
    /// 1. 读取决策携带的 [`BudgetSnapshot`] 并转换为 [`SubscriptionBudget`]，以统一数值精度；
    /// 2. 若决策为 `Exhausted`，直接返回 `BudgetExhausted`；
    /// 3. 若决策为 `Granted`，检查剩余额度：
    ///    - `remaining == 0`：说明此次消费耗尽额度，应立即返回 `BudgetExhausted`；
    ///    - 否则表示预算仍可继续使用，返回 `Ready`。
    ///
    /// ## 设计考量（Trade-offs & Gotchas）
    /// - 采用快照而非重新查询预算，避免二次读取引入竞态；
    /// - 由于快照使用 `u64` 表示剩余值，转换为 `SubscriptionBudget` 时会饱和截断到 `u32`，
    ///   当预算超过 `u32::MAX` 时调用方若需精确值，应保留原始快照用于日志；
    /// - 若未来扩展 `BudgetDecision` 新分支，应在此同步更新映射逻辑，保持“预算耗尽 → BudgetExhausted”的唯一语义。
    pub fn from_budget_decision(decision: &BudgetDecision) -> Self {
        match decision {
            BudgetDecision::Exhausted { snapshot } => {
                ReadyState::BudgetExhausted(SubscriptionBudget::from(snapshot))
            }
            BudgetDecision::Granted { snapshot } => {
                let budget = SubscriptionBudget::from(snapshot);
                if budget.remaining == 0 {
                    ReadyState::BudgetExhausted(budget)
                } else {
                    ReadyState::Ready
                }
            }
        }
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
    use crate::contract::{BudgetDecision, BudgetKind, BudgetSnapshot};

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

    /// 验证：预算决策为 Exhausted 时返回 `BudgetExhausted`。
    #[test]
    fn budget_decision_exhausted_maps_to_budget_state() {
        let snapshot = BudgetSnapshot::new(BudgetKind::Flow, 0, 5);
        let decision = BudgetDecision::Exhausted { snapshot };
        let status = ReadyState::from_budget_decision(&decision);

        assert!(matches!(
            status,
            ReadyState::BudgetExhausted(SubscriptionBudget {
                limit: 5,
                remaining: 0
            })
        ));
    }

    /// 验证：预算仍有剩余时返回 `Ready`。
    #[test]
    fn budget_decision_granted_with_remaining_maps_to_ready() {
        let snapshot = BudgetSnapshot::new(BudgetKind::Flow, 3, 5);
        let decision = BudgetDecision::Granted { snapshot };
        let status = ReadyState::from_budget_decision(&decision);

        assert_eq!(status, ReadyState::Ready);
    }

    /// 验证：最后一次成功消费后额度为 0，也应返回 `BudgetExhausted`。
    #[test]
    fn budget_decision_granted_zero_remaining_maps_to_budget_state() {
        let snapshot = BudgetSnapshot::new(BudgetKind::Flow, 0, 5);
        let decision = BudgetDecision::Granted { snapshot };
        let status = ReadyState::from_budget_decision(&decision);

        assert!(matches!(
            status,
            ReadyState::BudgetExhausted(SubscriptionBudget {
                limit: 5,
                remaining: 0
            })
        ));
    }
}
