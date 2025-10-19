use crate::SparkError;
use alloc::{borrow::Cow, boxed::Box, vec::Vec};

use super::WritableBuffer;

/// `BufferPool` 规定缓冲区租借与回收的统一接口。
///
/// # 设计背景（Why）
/// - 综合 Netty `ByteBufAllocator`、Envoy `WatermarkBufferFactory`、Aerospike `BufferPool`、ClickHouse `Arena`、Tokio `BytesMut` 共享池实践，确保在高并发场景稳定控制内存峰值。
/// - 运行时、传输层、协议层需要共享统一的租借来源，以支持跨线程协程协作和背压策略。
///
/// # 逻辑解析（How）
/// - `acquire` 负责租借指定最小容量的可写缓冲，底层可采用 slab、分级自由链表或 jemalloc arena。
/// - `shrink_to_fit` 允许主动归还多余容量，吸收 Rust `Vec::shrink_to_fit` 与 Netty `trimUnreleasedBytes` 的思路。
/// - `statistics` 提供池化观测指标，便于运维与自适应扩容。
///
/// # 契约说明（What）
/// - **输入参数**：`min_capacity` 表示调用方当前写入批次最少需要的字节数。
/// - **返回值**：
///   - `acquire` 返回实现了 [`WritableBuffer`] 的缓冲实例，调用方拥有唯一所有权。
///   - `shrink_to_fit` 返回实际回收的字节数，便于调用方做指标打点。
///   - `statistics` 返回 [`PoolStats`] 快照，可为空实现但建议填充核心字段。
/// - **前置条件**：池实现必须是线程安全的；若依赖外部内存池，需确保生命周期覆盖整个运行时。
/// - **后置条件**：`acquire` 成功后必须保证返回缓冲的 `remaining_mut() >= min_capacity`。
///
/// # 设计考量（Trade-offs & Gotchas）
/// - **分配策略**：允许实现根据场景选择固定大小、指数级或 TCMalloc 风格的分配策略，契约仅关注语义。
/// - **背压处理**：当池容量不足时可返回 `SparkError`，或在错误中携带降级建议（如切换到 on-heap）。
/// - **观测性**：`statistics` 默认返回静态数据，鼓励实现者提供诸如“池使用率”“活跃租借数”等核心指标。
pub trait BufferPool: Send + Sync + 'static {
    /// 租借一个最少具备 `min_capacity` 可写空间的缓冲区。
    fn acquire(&self, min_capacity: usize) -> Result<Box<dyn WritableBuffer>, SparkError>;

    /// 主动收缩池内冗余内存，返回实际回收的字节数。
    fn shrink_to_fit(&self) -> Result<usize, SparkError>;

    /// 返回池当前的核心统计指标（例如 `usage_bytes`、`lease_count`）。
    fn statistics(&self) -> Result<PoolStats, SparkError>;
}

/// 池统计快照，帮助调用方观测内存行为并执行自适应调度。
///
/// # 设计背景（Why）
/// - 旧版接口通过 `&dyn PoolStatisticsView` + `as_pairs` 返回键值对数组，
///   对于常见的容量、租借数等核心指标缺乏静态类型，调用方只能通过字符串匹配取值，
///   难以编译期校验字段是否存在，且在 Prometheus/OpenTelemetry 等后端中无法直接做强类型映射。
/// - 新结构体以明确字段呈现核心指标，并保留 `custom_dimensions` 承载实现特有的数据，
///   在保证可扩展性的同时提升类型安全与 IDE 可发现性。
///
/// # 逻辑解析（How）
/// - `PoolStats` 使用值语义（`Clone` + `Default`），实现者可在内部生成快照后返回，
///   调用方无需担心生命周期绑定，实现上常见做法是读取原子计数或持锁收集数据。
/// - `custom_dimensions` 采用 `Vec<PoolStatDimension>`，鼓励实现者使用稳定的蛇形命名键，
///   并可通过该列表扩展如分级池使用率等信息。
///
/// # 契约说明（What）
/// - **字段含义**：
///   - `allocated_bytes`：池向系统请求的总字节数，包含已借出与待命容量。
///   - `resident_bytes`：当前常驻内存（通常等于或小于 `allocated_bytes`），可辅助判断碎片。
///   - `active_leases`：正在被调用方持有的缓冲数量。
///   - `available_bytes`：无需再分配即可提供的剩余容量，用于驱动背压或预扩容逻辑。
///   - `pending_lease_requests`：处于等待/排队状态的租借请求数量，帮助识别热点。
///   - `failed_acquisitions`：累计租借失败次数，方便实现者暴露降级建议。
///   - `custom_dimensions`：实现自定义指标的有序列表，键建议使用 `snake_case` 并保持稳定。
/// - **前置条件**：实现者在构造快照时需保证字段语义一致（例如 `available_bytes <= allocated_bytes`）。
/// - **后置条件**：返回的结构体代表调用瞬间的快照，不应长期引用内部可变状态。
///
/// # 设计取舍与风险（Trade-offs）
/// - 采用 `usize`/`u64` 而非 `NonZero` 类型，以支持尚未初始化或暂不可用时返回 0；
///   调用方若需要严格不为 0，可自行断言。
/// - `custom_dimensions` 仍使用字符串键，允许实验性指标快速落地；
///   若需进一步约束，可在上层定义白名单或枚举映射。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    pub allocated_bytes: usize,
    pub resident_bytes: usize,
    pub active_leases: usize,
    pub available_bytes: usize,
    pub pending_lease_requests: usize,
    pub failed_acquisitions: u64,
    pub custom_dimensions: Vec<PoolStatDimension>,
}

/// 扩展指标维度，用于承载实现者的定制数据。
///
/// # 契约说明（What）
/// - `key`：稳定的蛇形命名字符串，建议使用模块前缀（例如 `slab_spans`）。
/// - `value`：非负整数值，可表示计数或容量；若需记录浮点，请在上层转换为基准单位。
/// - **前置条件**：键必须对同一实现保持稳定，避免调用方缓存后发生语义变化。
/// - **后置条件**：维度序列可为空；如需表示比率等小数，可通过扩展字段或编码为百分比整数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolStatDimension {
    pub key: Cow<'static, str>,
    pub value: usize,
}
