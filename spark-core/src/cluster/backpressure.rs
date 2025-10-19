use crate::BoxStream;
use alloc::sync::Arc;
use core::{fmt, num::NonZeroUsize};

/// 分布式订阅使用的溢出处理策略。
///
/// # 设计背景（Why）
/// - 结合 Kafka、etcd、NATS 等系统的经验，总结最常用的三类溢出策略：丢弃最旧、丢弃最新、直接失败。
/// - 统一语义可帮助调用方跨实现编写稳定的恢复逻辑（例如感知丢弃并回滚修订号）。
///
/// # 契约说明（What）
/// - `DropOldest`：优先保证最新事件，适合状态收敛速度要求高的场景。
/// - `DropNewest`：优先保证完整的历史，常用于日志/审计场景。
/// - `FailStream`：一旦触发溢出即终止 Stream，并返回 `cluster.queue_overflow` 错误。
///
/// # 风险提示（Trade-offs）
/// - 不同实现可能对溢出检测粒度不同（按事件、按批次），调用方应结合 [`SubscriptionQueueProbe`] 做运行期观测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    DropOldest,
    DropNewest,
    FailStream,
}

/// 背压模式，描述订阅内部使用的缓冲模型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackpressureMode {
    /// 无界缓冲，由实现自行决定溢出策略（可能退化为阻塞/等待）。
    Unbounded,
    /// 有界缓冲 + 指定溢出策略。
    Bounded {
        capacity: NonZeroUsize,
        overflow: OverflowPolicy,
    },
}

/// 订阅背压配置，调用方可通过构造函数表达期望行为。
///
/// # 契约说明（What）
/// - `mode`：缓冲容量与溢出策略，默认 `Unbounded`。
/// - `observe_queue`：是否请求实现暴露队列探针；若为 `true`，实现应在返回的 [`SubscriptionStream`] 中填充 `queue_probe`。
/// - **前置条件**：当 `mode` 为 `Bounded` 时，`capacity` 必须大于 0；建议根据事件大小设置合理数值。
/// - **后置条件**：实现接收到该配置后应尽力遵守；若无法满足，应在订阅流的第一条事件或错误中说明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionBackpressure {
    pub mode: BackpressureMode,
    pub observe_queue: bool,
}

impl SubscriptionBackpressure {
    /// 构造一个默认的无界配置。
    pub const fn unbounded() -> Self {
        Self {
            mode: BackpressureMode::Unbounded,
            observe_queue: false,
        }
    }

    /// 构造一个带容量限制的配置。
    pub const fn bounded(capacity: NonZeroUsize, overflow: OverflowPolicy) -> Self {
        Self {
            mode: BackpressureMode::Bounded { capacity, overflow },
            observe_queue: false,
        }
    }

    /// 请求实现提供队列观测能力。
    pub const fn with_queue_observation(mut self, enable: bool) -> Self {
        self.observe_queue = enable;
        self
    }
}

impl Default for SubscriptionBackpressure {
    fn default() -> Self {
        Self::unbounded()
    }
}

/// 订阅流的返回体，承载事件流与可选的队列探针。
///
/// # 契约说明（What）
/// - `stream`：核心事件流，语义与旧版 `BoxStream` 返回值一致。
/// - `queue_probe`：若调用方通过配置请求观测能力，且实现支持，则提供该探针；调用方可周期性调用 `snapshot` 读取深度和丢弃次数。
/// - **前置条件**：实现需保证 `queue_probe` 与 `stream` 生命周期一致；当流结束时，应视为探针数据不再更新。
/// - **后置条件**：调用方可在消费流的同时使用探针对接指标系统，实现实时背压调优。
pub struct SubscriptionStream<T> {
    pub stream: BoxStream<'static, T>,
    pub queue_probe: Option<Arc<dyn SubscriptionQueueProbe>>,
}

impl<T> SubscriptionStream<T> {
    /// 仅使用事件流构建订阅返回体。
    pub fn new(stream: BoxStream<'static, T>) -> Self {
        Self {
            stream,
            queue_probe: None,
        }
    }

    /// 为订阅附加队列探针。
    pub fn with_probe(mut self, probe: Arc<dyn SubscriptionQueueProbe>) -> Self {
        self.queue_probe = Some(probe);
        self
    }
}

impl<T> fmt::Debug for SubscriptionStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionStream")
            .field("has_probe", &self.queue_probe.is_some())
            .finish()
    }
}

/// 订阅队列的观测快照。
///
/// # 契约说明（What）
/// - `capacity`：若存在容量上限则返回 `Some`；无界时为 `None`。
/// - `depth`：当前队列中待消费事件数量。
/// - `dropped_events`：自订阅建立以来的累计丢弃事件数。
/// - **后置条件**：实现应保证同一订阅的快照 `depth` 是单调变化的真实值或近似上界（允许存在读取延迟）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubscriptionQueueSnapshot {
    pub capacity: Option<NonZeroUsize>,
    pub depth: usize,
    pub dropped_events: u64,
}

/// 订阅队列探针，用于查询当前背压状态。
///
/// # 契约说明（What）
/// - `snapshot`：应返回最新已知状态，调用成本应足够低以支持高频查询。
/// - **前置条件**：实现需保证该方法线程安全；若查询代价高昂，可在实现内部做缓存。
/// - **后置条件**：若订阅已终止，允许继续返回最后一次快照，也可选择在内部标记并返回零值。
pub trait SubscriptionQueueProbe: Send + Sync + 'static {
    fn snapshot(&self) -> SubscriptionQueueSnapshot;
}
