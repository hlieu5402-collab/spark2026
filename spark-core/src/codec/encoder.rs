use super::metadata::CodecDescriptor;
use crate::SparkError;
use crate::buffer::{BufferAllocator, ErasedSparkBuf, ErasedSparkBufMut};
use alloc::boxed::Box;
use core::fmt;

/// `EncodeContext` 为编码过程提供共享资源视图。
///
/// # 设计背景（Why）
/// - 借鉴 Tokio `Codec` 与 Netty `ChannelHandlerContext` 思路，将缓冲租借、预算控制等能力外置，使编码实现聚焦于序列化细节。
/// - 在无标准库场景下，统一从 `BufferAllocator` 租借可写缓冲，避免直接依赖 `Vec<u8>` 导致内存碎片。
///
/// # 逻辑解析（How）
/// - `new` 仅要求传入分配器；`with_max_frame_size` 允许额外携带帧大小预算，用于限制极端输入。
/// - `acquire_buffer` 是访问分配器的便捷方法，便于实现者撰写无 `unwrap` 的安全代码。
/// - `max_frame_size` 暴露预算，编码器可据此切片或启用分块传输。
///
/// # 契约说明（What）
/// - **前置条件**：分配器必须实现线程安全，且返回的缓冲区满足 `Send + Sync` 要求。
/// - **后置条件**：成功租借的缓冲区由调用方负责归还或冻结为只读缓冲；上下文自身不持有状态。
///
/// # 风险提示（Trade-offs）
/// - 未内置重试逻辑；若租借失败（例如内存池耗尽），编码器需向上返回 `SparkError`，由调用者决定降级或背压。
pub struct EncodeContext<'a> {
    allocator: &'a dyn BufferAllocator,
    max_frame_size: Option<usize>,
}

impl<'a> EncodeContext<'a> {
    /// 使用分配器构建上下文。
    pub fn new(allocator: &'a dyn BufferAllocator) -> Self {
        Self {
            allocator,
            max_frame_size: None,
        }
    }

    /// 附带可选的最大帧尺寸约束。
    pub fn with_max_frame_size(
        allocator: &'a dyn BufferAllocator,
        max_frame_size: Option<usize>,
    ) -> Self {
        Self {
            allocator,
            max_frame_size,
        }
    }

    /// 租借一个满足最小容量的可写缓冲区。
    pub fn acquire_buffer(
        &self,
        min_capacity: usize,
    ) -> Result<Box<ErasedSparkBufMut>, SparkError> {
        self.allocator.acquire(min_capacity)
    }

    /// 返回最大帧尺寸预算。
    pub fn max_frame_size(&self) -> Option<usize> {
        self.max_frame_size
    }
}

impl fmt::Debug for EncodeContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodeContext")
            .field("max_frame_size", &self.max_frame_size)
            .finish()
    }
}

/// `EncodedPayload` 表示编码完成、可沿管道传播的字节缓冲。
///
/// # 设计背景（Why）
/// - 类似 gRPC、NATS 中的帧抽象，编码器需要返回只读视图以便下游传输层直接写出。
/// - 通过持有 `Box<ErasedSparkBuf>`，允许实现者提供自定义引用计数缓冲或零拷贝切片。
///
/// # 逻辑解析（How）
/// - `from_buffer` 接收任何对象安全的缓冲实现。
/// - `into_buffer` 释放所有权，供调用方写入传输层或复用。
///
/// # 契约说明（What）
/// - **前置条件**：传入缓冲必须遵循 `ErasedSparkBuf` 的语义，读指针指向有效负载开头。
/// - **后置条件**：结构体不对缓冲执行额外处理，确保零拷贝传播。
///
/// # 风险提示（Trade-offs）
/// - 不提供部分写入标记；如需分片发送，应在更高层通过 `PipelineMessage::Buf` 的多帧组合实现。
pub struct EncodedPayload {
    buffer: Box<ErasedSparkBuf>,
}

impl EncodedPayload {
    /// 使用已经冻结的只读缓冲创建负载。
    pub fn from_buffer(buffer: Box<ErasedSparkBuf>) -> Self {
        Self { buffer }
    }

    /// 取回底层缓冲。
    pub fn into_buffer(self) -> Box<ErasedSparkBuf> {
        self.buffer
    }
}

impl fmt::Debug for EncodedPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedPayload")
            .field("remaining", &self.buffer.remaining())
            .finish()
    }
}

/// `Encoder` 定义将高层对象转换为字节流的契约。
///
/// # 设计背景（Why）
/// - 吸收 Netty `MessageToByteEncoder`、Tokio `Encoder` 与 gRPC 拓展点经验，统一了内容描述与上下文依赖。
/// - 通过关联类型 `Item` 表达输出对象，保证在静态类型系统中保持语义清晰。
///
/// # 逻辑解析（How）
/// - `descriptor` 返回编解码描述符，供注册中心或握手阶段使用。
/// - `encode` 负责将业务对象序列化为 `EncodedPayload`，并可根据上下文申请缓冲或检查帧预算。
///
/// # 契约说明（What）
/// - **前置条件**：调用方需确保输入 `item` 满足业务协议约束，例如字段完整性、schema 兼容性。
/// - **后置条件**：返回的 `EncodedPayload` 必须完全代表 `item` 的序列化结果；若发生错误，应通过 `SparkError` 携带稳定错误码。
///
/// # 风险提示（Trade-offs）
/// - 接口未强制 `&mut self`，方便实现者以无状态单例形式注册，但若内部需要缓存，应自行处理并发安全。
pub trait Encoder: Send + Sync + 'static {
    /// 编码输入的具体业务类型。
    type Item: Send + Sync + 'static;

    /// 返回与该编码器绑定的描述符。
    fn descriptor(&self) -> &CodecDescriptor;

    /// 将业务对象编码为字节负载。
    fn encode(
        &self,
        item: &Self::Item,
        ctx: &mut EncodeContext<'_>,
    ) -> Result<EncodedPayload, SparkError>;
}
