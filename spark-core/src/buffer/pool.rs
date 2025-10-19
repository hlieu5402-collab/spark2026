use crate::SparkError;
use alloc::boxed::Box;
use core::fmt;

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
///   - `statistics` 以键值对的形式暴露快照，可为空实现。
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
    fn statistics(&self) -> Result<&dyn PoolStatisticsView, SparkError>;
}

/// 只读的池统计视图，帮助调用方观测内存行为。
pub trait PoolStatisticsView: fmt::Debug + Send + Sync {
    /// 返回以键值对形式表示的快照，键为稳定的蛇形命名字符串。
    fn as_pairs(&self) -> &[(&'static str, usize)];
}
