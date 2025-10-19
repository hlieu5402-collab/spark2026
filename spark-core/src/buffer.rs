use crate::SparkError;
use alloc::{boxed::Box, vec::Vec};
use core::{any::Any, fmt};

/// `ErasedSparkBuf` 为流水线传递的对象安全可读缓冲区抽象。
///
/// # 设计背景（Why）
/// - 统一 L4/L5 的字节流表达，屏蔽具体缓冲实现（例如环形缓冲、引用计数缓冲）。
/// - 保持对象安全，便于通过 trait 对象在热路径中传播。
///
/// # 逻辑解析（How）
/// - 定义读取能力：`remaining`、`as_bytes`、`split_to`、`advance`。
/// - `split_to` 以 `Result<Box<dyn ErasedSparkBuf>, SparkError>` 返回，确保错误可携带统一错误码。
///
/// # 契约说明（What）
/// - **前置条件**：调用者需保证缓冲区实现内部具备线程安全或引用计数语义，以满足 `Send + Sync`。
/// - **后置条件**：`split_to` 必须维护剩余数据的正确性；`advance` 调用后读指针需前移。
///
/// # 风险提示（Trade-offs）
/// - 该抽象牺牲部分静态类型信息，以换取协议无关性；性能敏感场景可通过自定义实现进行优化。
pub trait ErasedSparkBuf: Send + Sync + 'static {
    /// 返回剩余可读字节数。
    fn remaining(&self) -> usize;

    /// 以切片方式暴露可读字节。
    fn as_bytes(&self) -> &[u8];

    /// 将前 `n` 字节拆分为新的缓冲区，并推进当前读指针。
    fn split_to(&mut self, n: usize) -> Result<Box<dyn ErasedSparkBuf>, SparkError>;

    /// 将读指针前移 `n` 字节。
    fn advance(&mut self, n: usize);
}

/// `ErasedSparkBufMut` 表示对象安全的可写缓冲区。
///
/// # 设计背景（Why）
/// - 服务端在编码或聚合响应时需对字节缓存进行统一管理，避免绑定具体实现。
///
/// # 逻辑解析（How）
/// - `reserve` 提前申请空间，`put_slice` 写入数据，`freeze` 将可写缓冲冻结为只读视图。
///
/// # 契约说明（What）
/// - **前置条件**：`reserve` 的 `min_capacity` 必须是应用可接受的最小容量；实现应至少保证该容量。
/// - **后置条件**：`freeze` 返回的新缓冲区应与之前写入的数据严格一致。
///
/// # 风险提示（Trade-offs）
/// - 在 `no_std` 环境中，常见实现需依赖自定义内存池；过度扩容可能导致碎片化，需要实现者自行管理。
pub trait ErasedSparkBufMut: Send + Sync + 'static {
    /// 当前可写容量。
    fn capacity(&self) -> usize;

    /// 确保缓冲区拥有至少 `min_capacity` 的可写空间。
    fn reserve(&mut self, min_capacity: usize) -> Result<(), SparkError>;

    /// 将 `src` 写入缓冲区末尾。
    fn put_slice(&mut self, src: &[u8]);

    /// 冻结缓冲区，返回只读视图。
    fn freeze(self: Box<Self>) -> Box<dyn ErasedSparkBuf>;
}

/// `BufferAllocator` 提供缓冲区池化分配能力。
///
/// # 设计背景（Why）
/// - 统一控制内存使用峰值，避免每次写入都创建临时 `Vec` 或 `Box<[u8]>`。
///
/// # 契约说明（What）
/// - **输入**：`min_capacity` 指定需要的最小容量。
/// - **输出**：返回的缓冲区应可立即写入且满足容量需求。
///
/// # 风险提示（Trade-offs）
/// - 如果池内资源耗尽，实现者需考虑阻塞、失败或降级策略，并通过 `SparkError` 反馈。
pub trait BufferAllocator: Send + Sync + 'static {
    /// 从池中租借一个可写缓冲区。
    fn acquire(&self, min_capacity: usize) -> Result<Box<dyn ErasedSparkBufMut>, SparkError>;
}

/// `PipelineMessage` 聚合 L4 字节与 L7 用户态消息。
///
/// # 设计背景（Why）
/// - 解决统一管道在处理原始字节与高层协议对象时的类型统一问题。
///
/// # 契约说明（What）
/// - `Buf` 变体携带字节视图；`User` 变体携带任意 `Send + Sync` 的业务消息。
/// - 管道实现需对 `User` 进行类型检查与下转型。
pub enum PipelineMessage {
    /// L4 字节缓冲。
    Buf(Box<dyn ErasedSparkBuf>),
    /// L7 业务消息。
    User(Box<dyn Any + Send + Sync>),
}

/// 辅助类型：实现者可选地将内部缓冲快照化为 `Vec<u8>`。
pub type Bytes = Vec<u8>;

impl fmt::Debug for PipelineMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineMessage::Buf(_) => f.debug_tuple("Buf").field(&"<erased-buffer>").finish(),
            PipelineMessage::User(_) => f.debug_tuple("User").field(&"<erased-user>").finish(),
        }
    }
}
