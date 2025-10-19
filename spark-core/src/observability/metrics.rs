use super::attributes::AttributeSet;
use alloc::sync::Arc;

/// 指标仪表的元数据描述。
///
/// # 设计背景（Why）
/// - 吸收 OpenTelemetry Instrument Descriptor 的设计，统一指标名称、描述与单位的声明方式。
/// - 通过借用方式传递元数据，避免在热路径重复分配字符串。
///
/// # 契约说明（What）
/// - `name` 必须遵循 `namespace.metric_name` 的蛇形命名，并保持全局唯一。
/// - `description` 建议提供人类可读说明，便于文档化；`unit` 遵循 UCUM 或惯用单位（如 `ms`、`bytes`）。
/// - **前置条件**：调用方需在创建仪表前完成命名规范审查，避免产生重复指标。
/// - **后置条件**：元数据仅在调用期间有效，实现方不得持久化引用；如需持久化应克隆为拥有所有权的副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrumentDescriptor<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub unit: Option<&'a str>,
}

impl<'a> InstrumentDescriptor<'a> {
    /// 构造元数据描述。
    pub const fn new(name: &'a str) -> Self {
        Self {
            name,
            description: None,
            unit: None,
        }
    }

    /// 附加说明文本。
    pub const fn with_description(mut self, description: &'a str) -> Self {
        self.description = Some(description);
        self
    }

    /// 附加单位信息。
    pub const fn with_unit(mut self, unit: &'a str) -> Self {
        self.unit = Some(unit);
        self
    }
}

/// 单调递增计数器。
///
/// # 设计背景（Why）
/// - 参照 Prometheus Counter、OpenTelemetry `Counter` 设计，记录事件发生次数或累计量。
///
/// # 契约说明（What）
/// - **前置条件**：`value` 必须非负，且调用方需保证单调递增语义。
/// - **后置条件**：实现必须在内部保证线程安全；若底层后端暂不可用，应丢弃或缓存数据而非阻塞热路径。
///
/// # 风险提示（Trade-offs）
/// - 高频调用建议批量聚合后再提交，避免对远端后端造成压力。
pub trait Counter: Send + Sync {
    /// 累加指标值。
    fn add(&self, value: u64, attributes: AttributeSet<'_>);

    /// 累加 1 的便捷方法。
    fn increment(&self, attributes: AttributeSet<'_>) {
        self.add(1, attributes);
    }
}

/// 可上下波动的测量仪表。
///
/// # 设计背景（Why）
/// - 对应 OpenTelemetry `UpDownCounter` 与 Prometheus Gauge，适用于连接数、队列长度等瞬时值。
///
/// # 契约说明（What）
/// - **前置条件**：`value` 可正可负，调用方需确保最终值在业务允许范围内。
/// - **后置条件**：实现需具备线程安全；建议提供无锁或细粒度锁优化。
///
/// # 风险提示（Trade-offs）
/// - 若底层实现使用互斥锁，在高频场景可能成为瓶颈；可结合原子类型或分片计数器优化。
pub trait Gauge: Send + Sync {
    /// 直接设置数值。
    fn set(&self, value: f64, attributes: AttributeSet<'_>);

    /// 增加数值。
    fn increment(&self, delta: f64, attributes: AttributeSet<'_>);

    /// 减少数值。
    fn decrement(&self, delta: f64, attributes: AttributeSet<'_>);
}

/// 直方图指标。
///
/// # 设计背景（Why）
/// - 对齐 OpenTelemetry `Histogram`，记录延迟、大小等分布数据，支持 P99 等百分位分析。
///
/// # 契约说明（What）
/// - **前置条件**：`value` 应满足业务语义（如延迟 >= 0）。
/// - **后置条件**：实现可选择固定桶、指数桶或高精度直方图，只要能保证顺序一致性。
///
/// # 风险提示（Trade-offs）
/// - 高精度配置可能带来内存与 CPU 开销；建议根据场景权衡桶数量。
pub trait Histogram: Send + Sync {
    /// 记录样本值。
    fn record(&self, value: f64, attributes: AttributeSet<'_>);
}

/// 指标提供者的抽象。
///
/// # 设计背景（Why）
/// - 统一各类后端（Prometheus、StatsD、OpenTelemetry Collector）的仪表创建流程。
/// - 兼容学术研究中的自定义后端，允许注入实验性聚合算法。
///
/// # 契约说明（What）
/// - **前置条件**：调用方在创建仪表前需提供符合规范的 [`InstrumentDescriptor`]。
/// - **后置条件**：返回的仪表实例应可长期使用；实现可缓存实例避免重复创建。
/// - **直接记录**：对于性能敏感路径，可使用 `record_*` 快捷方法，避免持有 `Arc` 并降低虚函数调用次数。
///
/// # 风险提示（Trade-offs）
/// - 后端初始化失败时，建议返回降级实现（如空操作仪表），而非直接 panic。
pub trait MetricsProvider: Send + Sync + 'static {
    /// 获取或创建单调递增计数器。
    fn counter(&self, descriptor: &InstrumentDescriptor<'_>) -> Arc<dyn Counter>;

    /// 获取或创建可上下波动的仪表。
    fn gauge(&self, descriptor: &InstrumentDescriptor<'_>) -> Arc<dyn Gauge>;

    /// 获取或创建直方图。
    fn histogram(&self, descriptor: &InstrumentDescriptor<'_>) -> Arc<dyn Histogram>;

    /// 直接记录计数器增量，避免在调用方持有 `Arc`。
    ///
    /// # 设计动机（Why）
    /// - 高频指标场景中，频繁克隆或持有 `Arc` 会增加引用计数开销；提供一次性记录接口允许实现者进行批处理或无锁聚合。
    ///
    /// # 契约说明（What）
    /// - **参数**：`value` 必须非负；`attributes` 与 `counter` 方法一致。
    /// - **默认实现**：回退到 `counter().add`，确保向后兼容。
    fn record_counter_add(
        &self,
        descriptor: &InstrumentDescriptor<'_>,
        value: u64,
        attributes: AttributeSet<'_>,
    ) {
        self.counter(descriptor).add(value, attributes);
    }

    /// 直接设置或调整 Gauge 数值。
    ///
    /// # 契约说明
    /// - `value` 表示目标值，若实现希望基于增量优化，可在内部缓存上一次值。
    /// - 默认实现调用 `gauge().set`。
    fn record_gauge_set(
        &self,
        descriptor: &InstrumentDescriptor<'_>,
        value: f64,
        attributes: AttributeSet<'_>,
    ) {
        self.gauge(descriptor).set(value, attributes);
    }

    /// 直接记录直方图样本。
    ///
    /// # 契约说明
    /// - `value` 必须满足业务预期（如延迟 >= 0）。
    /// - 默认实现调用 `histogram().record`。
    fn record_histogram(
        &self,
        descriptor: &InstrumentDescriptor<'_>,
        value: f64,
        attributes: AttributeSet<'_>,
    ) {
        self.histogram(descriptor).record(value, attributes);
    }
}
