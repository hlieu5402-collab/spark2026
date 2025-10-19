use crate::BoxFuture;
use core::time::Duration;

/// `MonotonicTimePoint` 以相对时间刻度表达单调时钟读数。
///
/// # 设计背景（Why）
/// - `std::time::Instant` 在 `no_std` 场景不可用。该结构提供与其等价的基本能力，
///   以满足跨平台运行时的延时与调度需求。
///
/// # 逻辑解析（How）
/// - 内部以自启动以来的偏移量（`Duration`）表示，避免依赖壁钟时间。
/// - 提供加减操作与饱和差值，确保在不同硬件计时分辨率下行为一致。
///
/// # 契约说明（What）
/// - **前置条件**：调用方需确保所有时间点都来自同一计时来源，避免跨源比较导致语义错误。
/// - **后置条件**：由 [`TimeDriver::now`] 返回的时间点可直接与该结构协作运算。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicTimePoint(Duration);

impl MonotonicTimePoint {
    /// 根据启动以来的偏移量构造时间点。
    pub fn from_offset(offset: Duration) -> Self {
        MonotonicTimePoint(offset)
    }

    /// 返回自启动以来的时间偏移。
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    /// 计算两个时间点的饱和差值。
    pub fn saturating_duration_since(&self, earlier: MonotonicTimePoint) -> Duration {
        self.0
            .checked_sub(earlier.0)
            .unwrap_or_else(|| Duration::from_secs(0))
    }

    /// 基于当前时间点创建新的偏移量。
    pub fn saturating_add(&self, delta: Duration) -> MonotonicTimePoint {
        MonotonicTimePoint(self.0.saturating_add(delta))
    }
}

/// `TimeDriver` 定义统一的计时与延时接口。
///
/// # 设计背景（Why）
/// - 吸收 Tokio `time::Driver`、Quiche `Timer` 等成熟实现，结合实时调度研究中的精确计时需求，
///   提供最小但可组合的时间原语。
///
/// # 逻辑解析（How）
/// - `now`：返回单调时钟读数。
/// - `sleep`：基于持续时间延时，常用于节流、超时与心跳。
/// - `sleep_until`：基于目标时间点延时，便于对齐周期性任务。
/// - 默认实现 `sleep_until` 通过比较当前时间与目标时间决定是否立即完成。
///
/// # 契约说明（What）
/// - **前置条件**：实现者必须保证 `now` 单调递增；否则延时语义将被破坏。
/// - **后置条件**：延时 Future 完成时，运行时应确保至少等待了指定时间间隔。
///
/// # 性能契约（Performance Contract）
/// - `sleep` 与 `sleep_until` 返回 [`BoxFuture`]，实现与调用方之间以对象安全通信；每次调用包含一次堆分配与虚表调度。
/// - 针对高频短延时（如定时心跳 1ms）可通过实现自定义 `TimeDriver`，在内部复用 `Box` 缓冲或提供泛型变体以避免分配。
/// - 若运行时具备 GAT 支持，可暴露额外的 `fn sleep_raw(&self, ...) -> impl Future` 供性能敏感组件绕过对象安全层。
///
/// # 风险提示（Trade-offs）
/// - `sleep_until` 默认实现使用 `saturating_duration_since`，在系统时钟回拨情况下会立即完成；
///   如需不同策略，可在实现中覆写该方法。
pub trait TimeDriver: Send + Sync + 'static {
    fn now(&self) -> MonotonicTimePoint;

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;

    fn sleep_until(&self, deadline: MonotonicTimePoint) -> BoxFuture<'static, ()> {
        let now = self.now();
        let wait = deadline.saturating_duration_since(now);
        self.sleep(wait)
    }
}
