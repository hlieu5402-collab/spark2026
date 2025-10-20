use crate::contract::{BudgetDecision, BudgetSnapshot};
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
/// - `Budget`：预算耗尽，需配合 [`BudgetDecision`] 进行下一步策略。
/// - `Custom`：扩展场景，需提供稳定字符串，建议遵循 `namespace.reason` 命名。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackpressureReason {
    /// 上游处理链路繁忙。
    Upstream,
    /// 下游或对端繁忙。
    Downstream,
    /// 预算耗尽，附带预算快照。
    Budget(BudgetSnapshot),
    /// 自定义原因，遵循稳定命名。
    Custom(Cow<'static, str>),
}

impl BackpressureReason {
    /// 将预算决策转换为背压原因。
    pub fn from_budget(decision: &BudgetDecision) -> Option<Self> {
        match decision {
            BudgetDecision::Exhausted { snapshot } => {
                Some(BackpressureReason::Budget(snapshot.clone()))
            }
            BudgetDecision::Granted { .. } => None,
        }
    }
}

impl fmt::Display for BackpressureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackpressureReason::Upstream => write!(f, "upstream busy"),
            BackpressureReason::Downstream => write!(f, "downstream busy"),
            BackpressureReason::Budget(snapshot) => write!(
                f,
                "budget exhausted: {:?} remaining {}/{}",
                snapshot.kind(),
                snapshot.remaining(),
                snapshot.limit()
            ),
            BackpressureReason::Custom(reason) => write!(f, "{}", reason),
        }
    }
}

/// `poll_ready` 统一返回值，兼容背压、预算与错误语义。
///
/// # 契约说明（What）
/// - `Ready`：立即可执行。
/// - `Busy`：暂时不可用，附带背压原因。
/// - `BudgetExhausted`：预算耗尽但不视为错误，便于调用方采用排队或降级策略。
/// - `Error`：不可恢复错误，应转化为 [`SparkError`](crate::SparkError) 或上层错误。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PollReady<E> {
    /// 下游准备就绪。
    Ready,
    /// 暂时忙碌。
    Busy { reason: BackpressureReason },
    /// 预算耗尽。
    BudgetExhausted { snapshot: BudgetSnapshot },
    /// 出现不可恢复错误。
    Error(E),
}

impl<E> PollReady<E> {
    /// 是否处于就绪状态。
    pub fn is_ready(&self) -> bool {
        matches!(self, PollReady::Ready)
    }

    /// 将错误映射为其他类型。
    pub fn map_error<T, F: FnOnce(E) -> T>(self, f: F) -> PollReady<T> {
        match self {
            PollReady::Ready => PollReady::Ready,
            PollReady::Busy { reason } => PollReady::Busy { reason },
            PollReady::BudgetExhausted { snapshot } => PollReady::BudgetExhausted { snapshot },
            PollReady::Error(err) => PollReady::Error(f(err)),
        }
    }
}
