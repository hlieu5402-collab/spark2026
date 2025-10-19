//! 缓冲区契约模块。
//!
//! # 模块架构（Why）
//! - 将读取、写入、分配与消息桥接拆分为独立子模块，对齐 Netty、Tokio Bytes 等主流框架的职责分离实践。
//! - 通过统一的 `ReadableBuffer`/`WritableBuffer` 契约隐藏底层实现差异，让上层管线与运行时解耦具体内存策略。
//!
//! # 设计总览（How）
//! - [`readable`] 定义高性能只读缓冲协议，涵盖 `split_to`、`peek`、`advance` 等核心操作。
//! - [`writable`] 提供可写缓冲协议，强调与只读视图之间的“冻结”转换。
//! - [`pool`] 约束缓冲池接口，使运行时能够租借/归还缓冲以控制内存峰值。
//! - [`message`] 描述流水线消息体，支持零拷贝字节与高层业务消息并存。
//!
//! # 命名共识（Consistency）
//! - 所有类型均避免使用特定业务前缀，遵循 Rust 与异步生态的惯用术语，便于与 Bytes/Tonic/Tokio 等生态互操作。

pub mod message;
pub mod pool;
pub mod readable;
pub mod writable;

use crate::SparkError;
use alloc::boxed::Box;

pub use message::{Bytes, PipelineMessage};
pub use pool::{BufferPool, PoolStatisticsView};
pub use readable::ReadableBuffer;
pub use writable::WritableBuffer;

/// 统一的只读缓冲类型擦除别名。
///
/// # 设计背景（Why）
/// - 编解码、传输等组件需要通过 trait 对象共享缓冲，而无需了解具体实现类型。
/// - 借鉴 Tokio Bytes `Buf` 的惯例，将 [`ReadableBuffer`] 直接作为对象安全别名，避免额外包装结构。
///
/// # 契约说明（What）
/// - **前置条件**：底层实现必须完全遵循 [`ReadableBuffer`] 的语义约束。
/// - **后置条件**：当以 `Box<ErasedSparkBuf>` 形式传播时，调用方可无缝调用所有 [`ReadableBuffer`] 方法。
///
/// # 风险提示（Trade-offs）
/// - 名称沿用历史遗留的 `Spark` 前缀以保持向后兼容；新模块建议直接使用 `ReadableBuffer` 或自定义命名。
pub type ErasedSparkBuf = dyn ReadableBuffer;

/// 统一的可写缓冲类型擦除别名。
///
/// # 设计背景（Why）
/// - 与上游只读别名配套，支撑编码与解码上下文在不知道具体类型的情况下返回可写缓冲。
/// - 避免额外的虚函数跳板，直接重用 [`WritableBuffer`] 的能力。
///
/// # 契约说明（What）
/// - **前置条件**：具体类型必须满足 [`WritableBuffer`] 要求。
/// - **后置条件**：`Box<ErasedSparkBufMut>` 可直接调用写入、扩容、冻结等方法，并在冻结后转化为 [`ErasedSparkBuf`]。
///
/// # 风险提示（Trade-offs）
/// - 类型别名不会提供额外静态保证；若需要约束特定实现，可在上层定义新 trait 封装。
pub type ErasedSparkBufMut = dyn WritableBuffer;

/// 缓冲分配器契约。
///
/// # 设计背景（Why）
/// - 对齐 Netty `ByteBufAllocator`、Envoy `Buffer::Factory` 等实践，统一缓冲租借入口，避免业务层直接依赖具体池实现。
/// - 通过 trait 对象抽象，兼容研究领域的自适应分配策略（如 NUMA 感知、RDMA 零拷贝）。
///
/// # 契约说明（What）
/// - `acquire(min_capacity)`：租借一个至少具有 `min_capacity` 字节可写空间的缓冲。
/// - **前置条件**：实现必须线程安全，且返回的缓冲满足 [`ErasedSparkBufMut`] 语义。
/// - **后置条件**：成功租借的缓冲所有权转移给调用方，调用方负责后续冻结或归还逻辑。
///
/// # 风险提示（Trade-offs）
/// - 契约仅定义最小能力，若需统计或回收接口，可在实现层引入组合 trait；未内置释放方法，建议依赖 `Drop`/引用计数语义。
pub trait BufferAllocator: Send + Sync + 'static {
    /// 租借满足最小容量的缓冲。
    fn acquire(&self, min_capacity: usize) -> Result<Box<ErasedSparkBufMut>, SparkError>;
}

impl<T> BufferAllocator for T
where
    T: BufferPool,
{
    fn acquire(&self, min_capacity: usize) -> Result<Box<ErasedSparkBufMut>, SparkError> {
        BufferPool::acquire(self, min_capacity)
    }
}
