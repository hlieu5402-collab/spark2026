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
/// - **兼容性**：默认实现回退到 `counter/gauge/histogram` 构造方法，以便旧版实现无需修改即可获得新接口。
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
    /// - 高频指标场景中，频繁克隆或持有 `Arc` 会增加引用计数与虚函数分发开销。
    /// - 提供一次性记录接口，使实现方能对热点路径采用批处理、线程本地缓冲或 lock-free 汇聚策略。
    /// - 在整体架构中，该方法补充了 `counter` 构造器，适用于“即用即抛”场景；共享调用路径与后端聚合层。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：`descriptor` 必须指向一个已注册或可惰性创建的计数器元数据；`value` 为非负增量；`attributes` 描述维度标签。
    /// - **前置条件**：调用方应确保指标名称符合 `namespace.metric` 规范，且在调用时后端已完成必要初始化。
    /// - **后置条件**：成功执行后，该增量会被提交至底层计数器；如实现选择缓冲策略，应保证最终至少一次送达。
    ///
    /// # 执行逻辑（How）
    /// - 默认实现调用 `counter().add`，以保持旧调用路径的行为一致。
    /// - 自定义实现可绕过 `Arc` 构造，直接定位后端缓冲区，从而减少一次虚函数派发。
    ///
    /// # 风险与权衡（Trade-offs）
    /// - 若实现绕过 `counter` 缓存，需自行处理并发同步；否则可能破坏计数器的单调性。
    /// - 对降级为空操作的实现，应同样覆盖该方法以保持性能一致。
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
    /// # 设计动机（Why）
    /// - Gauge 常用于实时快照，调用端往往只需临时写入一次，不值得保留 `Arc<dyn Gauge>`。
    /// - 提供直接写入接口，便于实现者用更轻量的数据结构（如原子变量或线程本地缓冲）聚合瞬时值。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：`value` 为目标浮点值；`attributes` 描述附加标签。
    /// - **前置条件**：调用前需保证 `descriptor` 对应的 Gauge 在语义上允许被覆盖；必要时需确保数值仍在业务允许范围。
    /// - **后置条件**：默认实现会调用 `set` 并立即提交；自定义实现可缓存待刷新状态，但需保证最终一致性。
    ///
    /// # 执行逻辑（How）
    /// - 默认实现委托给 `gauge().set`，沿用既有缓存策略。
    /// - 实现者可通过复写，直接定位 Gauge 实例或采用无锁写入方案。
    ///
    /// # 风险与权衡（Trade-offs）
    /// - 若使用延迟刷新的策略，需考虑并发读写对快照准确性的影响。
    /// - 大量不同标签组合下直接调用仍可能触发内部缓存扩容，建议结合标签池复用。
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
    /// # 设计动机（Why）
    /// - 直方图通常用于延迟、大小的热点度量；在追求极致延迟时，希望避免为每次记录获取 `Arc`。
    /// - 允许实现者直接写入采样缓存或指数桶聚合器，从而减少锁竞争。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：`value` 为待记录样本，必须符合业务约束（如延迟必须非负）。
    /// - **前置条件**：调用方需确保指标描述符与直方图定义匹配（桶配置、单位等）。
    /// - **后置条件**：默认实现立即记录样本；若实现选择延迟聚合，应保证刷写时保持顺序一致性。
    ///
    /// # 执行逻辑（How）
    /// - 默认实现直接复用 `histogram().record`，无需额外适配。
    /// - 可以通过重写实现自定义缓冲，例如 per-thread reservoir 或 HdrHistogram。
    ///
    /// # 风险与权衡（Trade-offs）
    /// - 自定义实现必须考虑高吞吐下的桶溢出与内存使用。
    /// - 如采用抽样策略，应在文档中注明可能的精度损失。
    fn record_histogram(
        &self,
        descriptor: &InstrumentDescriptor<'_>,
        value: f64,
        attributes: AttributeSet<'_>,
    ) {
        self.histogram(descriptor).record(value, attributes);
    }
}
