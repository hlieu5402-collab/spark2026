use core::time::Duration;

/// 记录背压判定需要的关键运行时指标。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 汇总“当前写缓冲深度”“连续 WouldBlock 次数”“最近一次成功写入距离现在的时间”等指标，
///   为统一背压决策提供输入。
/// - 避免不同实现各自定义结构导致的语义错位，使调度器或监控组件能够跨协议复用同一决策逻辑。
///
/// ## 契约说明（What）
/// - `in_flight_ops`：当前尚未完成的写操作数量。
/// - `pending_bytes`：尚待刷出的字节数。
/// - `consecutive_would_block`：近期连续遭遇 `WouldBlock` 的次数。
/// - `elapsed_since_flush`：距上一次成功 `flush` 的时间。
/// - **前置条件**：调用者应按真实观测数据填充，字段均以零为默认。
/// - **后置条件**：该结构仅作为快照，不持久化内部状态。
///
/// ## 风险与注意事项（Trade-offs）
/// - 该结构未绑定具体协议，可根据实际需要忽略某些字段；
/// - 若实现缺乏某项指标，可保持默认值并在文档中说明影响范围。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BackpressureMetrics {
    /// 当前尚未完成的写操作数量。
    pub in_flight_ops: u32,
    /// 待冲刷字节数，供实现映射为窗口占用或队列深度。
    pub pending_bytes: u64,
    /// 连续遭遇 `WouldBlock` 的次数。
    pub consecutive_would_block: u32,
    /// 距离最近一次成功刷新的时间。
    pub elapsed_since_flush: Option<Duration>,
}

/// 背压决策结果，供调度器或调用方消费。
///
/// # 合同（What）
/// - `Ready`：当前可立即写入；
/// - `Busy`：存在轻度拥塞，可适当退避但无需立即失败；
/// - `RetryAfter`：建议等待给定时间后再尝试；
/// - `BudgetExhausted`：预算耗尽，需要上层补充；
/// - `Rejected`：实现无法继续接收数据，应视为硬失败。
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum BackpressureDecision {
    Ready,
    Busy,
    RetryAfter { delay: Duration },
    BudgetExhausted,
    Rejected,
}

impl BackpressureDecision {
    /// 判断决策是否允许立即写入。
    pub fn is_ready(self) -> bool {
        matches!(self, BackpressureDecision::Ready)
    }
}

/// 背压分类器接口，将观测指标映射为统一决策。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 提供策略插拔点，使不同协议可以采用阈值、指数退避或自适应算法而无需修改上层签名。
/// - 支持在 TCK 或生产环境中注入自定义策略（例如实验性拥塞算法）。
///
/// ## 契约说明（What）
/// - `classify`：根据指标返回 [`BackpressureDecision`]；
/// - **前置条件**：指标来自同一连接的最新采样；
/// - **后置条件**：决策结果作为只读信号，不会修改输入指标。
///
/// ## 风险提示（Trade-offs）
/// - 策略实现应避免在 `classify` 中执行阻塞操作，以免拖慢热路径；
/// - 若算法需要维护状态，请在实现内部持有共享结构，避免修改输入快照。
pub trait BackpressureClassifier: Send + Sync {
    /// 根据指标计算背压决策。
    fn classify(&self, metrics: &BackpressureMetrics) -> BackpressureDecision;
}
