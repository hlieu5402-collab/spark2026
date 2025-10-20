use crate::contract::BudgetDecision;
use crate::status::ready::{ReadyState, SubscriptionBudget};
use alloc::borrow::Cow;
use core::fmt;

/// 背压原因，统一透传 `Service`、编解码器、传输层之间的忙碌状态。
///
/// # 设计背景（Why）
/// - 早期实现采用 `Result<(), Error>` 表达 `poll_ready`，无法区分“暂时忙碌”和“永久失败”。
/// - 统一原因枚举后，调用方可以据此打点、回退、或在测试中模拟不同背压场景。
///
/// # 契约说明（What）
/// - `Upstream`：上游链路繁忙（如服务线程池饱和）。
/// - `Downstream`：下游或对端施加背压（如窗口耗尽）。
/// - `Custom`：扩展场景，需提供稳定字符串，建议遵循 `namespace.reason` 命名。
///
/// # 可见性与弃用策略
/// - **意图 (Why)**：当前 crate 对外改用 `ReadyState` 表达背压语义，本枚举仅作为历史迁移期的内部兼容桥梁，
///   因此保持 `pub(crate)` 可见性并同时应用 `#[allow(dead_code)]` 抑制编译器警告。
/// - **架构作用 (Where)**：位于 `spark-core::backpressure` 模块，供老版本桥接代码和测试夹具访问；
///   若未来完全移除旧路径，可删除本枚举并同步更新一致性脚本。
/// - **后置条件 (What)**：保持 crate 内部仍可引用该类型，同时保证不会再次对外暴露第二套背压 API。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
#[allow(dead_code)]
pub(crate) enum BackpressureReason {
    /// 上游处理链路繁忙。
    Upstream,
    /// 下游或对端繁忙。
    Downstream,
    /// 自定义原因，遵循稳定命名。
    Custom(Cow<'static, str>),
}

impl BackpressureReason {
    /// 将预算决策转换为统一的就绪状态。
    ///
    /// # 教案式说明
    /// - **意图 (Why)**：历史上预算耗尽通过 `BackpressureReason::Budget` 暴露，
    ///   但随着 `ReadyState::BudgetExhausted` 引入，我们希望调用方直接依据
    ///   `ReadyState` 触发降级或排队策略，避免背压模块维护平行的状态体系。
    /// - **契约 (What)**：
    ///   - **输入**：`decision` 必须来自 [`Budget::try_consume`](crate::contract::Budget::try_consume)
    ///     或等效逻辑；
    ///   - **输出**：当决策为 `Exhausted` 时返回 `Some(ReadyState::BudgetExhausted(_))`，
    ///     其它情况返回 `None` 以保持原语义；
    ///   - **前置条件**：调用方应确保预算决策与当前调用上下文一致，避免交叉污染；
    ///   - **后置条件**：本函数不会修改预算，仅完成状态映射。
    /// - **实现 (How)**：
    ///   1. 匹配决策枚举；
    ///   2. 在耗尽分支中调用 [`SubscriptionBudget::from`] 构造订阅预算快照；
    ///   3. 使用 `ReadyState::BudgetExhausted` 封装并返回。
    /// - **风险提示 (Trade-offs & Gotchas)**：
    ///   - `BudgetSnapshot` 的字段为 `u64`，我们在转换时使用饱和截断到 `u32`，
    ///     若原始预算超过 `u32::MAX` 将丢失精度；如需完整信息，请直接持有 `BudgetSnapshot`。
    /// # 兼容性策略
    /// - **意图 (Why)**：该函数目前主要服务于遗留调用链与契约测试，实际运行路径已逐步迁移至
    ///   `ReadyState`；为了避免 CI 在未引用时报告 `dead_code`，施加 `#[allow(dead_code)]` 并在说明中
    ///   明确此为过渡性工具函数。
    /// - **前置条件**：调用方须传入由预算决策逻辑返回的引用，且与当前上下文一致；
    /// - **后置条件**：不会修改输入，仅返回映射后的 `ReadyState` 或 `None`；
    /// - **注意事项 (Trade-offs)**：长期计划是直接在各调用点使用 `ReadyState::BudgetExhausted`，届时本函数
    ///   可安全移除。
    #[allow(dead_code)]
    pub fn budget_ready_state(decision: &BudgetDecision) -> Option<ReadyState> {
        match decision {
            BudgetDecision::Exhausted { snapshot } => Some(ReadyState::BudgetExhausted(
                SubscriptionBudget::from(snapshot),
            )),
            BudgetDecision::Granted { .. } => None,
        }
    }
}

impl fmt::Display for BackpressureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackpressureReason::Upstream => write!(f, "upstream busy"),
            BackpressureReason::Downstream => write!(f, "downstream busy"),
            BackpressureReason::Custom(reason) => write!(f, "{}", reason),
        }
    }
}
