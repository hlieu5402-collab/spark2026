use super::metadata::CodecDescriptor;
use crate::SparkError;
use crate::buffer::{BufferAllocator, ErasedSparkBuf, ErasedSparkBufMut};
use alloc::boxed::Box;
use core::fmt;

/// `DecodeContext` 为增量解码提供辅助资源。
///
/// # 设计背景（Why）
/// - 结合 gRPC streaming、QUIC 帧解析与 Kafka Fetch API 设计，解码过程往往需要临时缓冲或预算信息，统一上下文便于适配多协议。
/// - 在 `no_std` 环境下，避免直接依赖全局分配，保障在嵌入式或多租户场景稳定运行。
///
/// # 逻辑解析（How）
/// - `new` 接收缓冲分配器；`with_max_frame_size` 提供帧预算，防止畸形包攻击。
/// - `acquire_scratch` 允许解码器申请可写缓冲，常用于重组分片或执行临时拷贝。
/// - `max_frame_size` 帮助实现根据预算提前拒绝或调整策略。
///
/// # 契约说明（What）
/// - **前置条件**：分配器返回的缓冲区必须满足 `Send + Sync`，以保证解码器在多线程环境安全复用。
/// - **后置条件**：上下文本身不保存状态，调用方可在多次解码之间重复使用同一实例。
///
/// # 风险提示（Trade-offs）
/// - 仅提供最小约束；若需要统计信息（如包计数、速率），应在更高层扩展。
pub struct DecodeContext<'a> {
    allocator: &'a dyn BufferAllocator,
    max_frame_size: Option<usize>,
}

impl<'a> DecodeContext<'a> {
    /// 使用分配器构建解码上下文。
    pub fn new(allocator: &'a dyn BufferAllocator) -> Self {
        Self {
            allocator,
            max_frame_size: None,
        }
    }

    /// 附加可选帧预算信息。
    pub fn with_max_frame_size(
        allocator: &'a dyn BufferAllocator,
        max_frame_size: Option<usize>,
    ) -> Self {
        Self {
            allocator,
            max_frame_size,
        }
    }

    /// 租借一块临时可写缓冲，用于重组片段或复制数据。
    pub fn acquire_scratch(
        &self,
        min_capacity: usize,
    ) -> Result<Box<dyn ErasedSparkBufMut>, SparkError> {
        self.allocator.acquire(min_capacity)
    }

    /// 返回最大帧预算。
    pub fn max_frame_size(&self) -> Option<usize> {
        self.max_frame_size
    }
}

impl fmt::Debug for DecodeContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodeContext")
            .field("max_frame_size", &self.max_frame_size)
            .finish()
    }
}

/// `DecodeOutcome` 表示一次尝试解码的结果状态。
///
/// # 设计背景（Why）
/// - 借鉴 Tokio `Decoder` 的 `Some/None` 语义与 Kafka streaming 的“拉取更多数据”模式，通过枚举明确区分三种常见状态。
/// - 额外提供 `Skipped`，以适配多路复用时对无关帧的快速丢弃需求。
///
/// # 逻辑解析（How）
/// - `Complete(T)`：成功生成一个业务对象；
/// - `Incomplete`：输入不足，调用方应在收到更多字节后重试；
/// - `Skipped`：帧被忽略，通常用于探测包或遥测数据。
///
/// # 契约说明（What）
/// - **前置条件**：解码器必须在消费字节后确保缓冲读指针正确推进；若返回 `Incomplete`，不得提前消费超出需求的字节。
/// - **后置条件**：`Complete` 分支返回的值必须满足业务协议定义。
///
/// # 风险提示（Trade-offs）
/// - 状态枚举未区分可恢复与不可恢复错误；若遇不可解析数据，应直接返回 `SparkError`。
#[derive(Debug)]
pub enum DecodeOutcome<T> {
    /// 成功解析出完整对象。
    Complete(T),
    /// 数据不足，等待更多输入。
    Incomplete,
    /// 解码器主动跳过此帧（通常依据业务约定）。
    Skipped,
}

/// `Decoder` 定义将字节流还原为高层对象的契约。
///
/// # 设计背景（Why）
/// - 与 Kafka `Deserializer`、gRPC `Codec` 解码侧保持一致，强调幂等与流式处理能力。
/// - 通过关联类型 `Item` 表示输出对象，确保静态类型安全。
///
/// # 逻辑解析（How）
/// - `descriptor` 与编码端一致，便于校验双方契约。
/// - `decode` 接收可变引用缓冲区，可直接调用 `split_to` 或 `advance` 控制读指针。
///
/// # 契约说明（What）
/// - **前置条件**：调用前应确保缓冲区包含连续字节；若消息跨帧，需要在上层聚合后再调用。
/// - **后置条件**：返回 `Complete` 时，解码器必须已经消费对应字节，避免重复解析；返回 `Incomplete` 时，应保持缓冲状态可回滚。
///
/// # 风险提示（Trade-offs）
/// - 若实现内部维护状态（如分片缓存），需自行处理并发安全，契约未强制 `&mut self`。
pub trait Decoder: Send + Sync + 'static {
    /// 解码输出的业务类型。
    type Item: Send + Sync + 'static;

    /// 返回绑定的描述符。
    fn descriptor(&self) -> &CodecDescriptor;

    /// 尝试将字节缓冲解码为高层对象。
    fn decode(
        &self,
        src: &mut dyn ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> Result<DecodeOutcome<Self::Item>, SparkError>;
}
