use super::ReadableBuffer;
use alloc::{boxed::Box, vec::Vec};
use core::{any::Any, fmt};

/// `PipelineMessage` 统一承载网络层字节与业务层对象。
///
/// # 设计背景（Why）
/// - 借鉴 Netty `ChannelPipeline`、Akka Stream `ByteString`、NATS `Message`、gRPC `Message`、Apache Pulsar `Message` 的复合消息模式，确保在一个通道内安全穿梭不同层级的数据。
/// - 框架内部需要在协议编解码、负载均衡、服务治理之间传递异构数据，因此需通过 trait 对象屏蔽具体类型。
///
/// # 逻辑解析（How）
/// - `Buffer` 变体封装 [`ReadableBuffer`]，用于承载 L4/L5 字节流，适配零拷贝与池化策略。
/// - `User` 变体封装任意 `Send + Sync` 对象，对应 L7 业务语义；通过 `Any` 支持运行时下转型。
/// - `Bytes` 类型别名提供轻量级 `Vec<u8>` 快照，方便边缘组件或测试用例无需实现 `ReadableBuffer` 也能消费数据。
///
/// # 契约说明（What）
/// - **前置条件**：
///   - 创建 `User` 时调用方必须保证内部类型满足线程安全语义。
///   - 流水线实现需要在消费 `User` 前进行类型判定并安全转换。
/// - **后置条件**：
///   - `Buffer` 变体在离开缓冲敏感区后应及时 `freeze` 或转换，以避免持有可变引用。
///   - `User` 变体不得假定存在默认实现，所有具体类型转换必须显式处理失败分支。
///
/// # 设计考量（Trade-offs & Gotchas）
/// - **对象擦除**：采用 `Any` 和 trait 对象实现，对比泛型消息牺牲了一定编译期优化，但能支持动态协议装配。
/// - **快照策略**：`Bytes` 仅作为临时快照，频繁使用会产生复制，应配合池化策略使用。
/// - **调试输出**：`Debug` 实现刻意隐藏内部细节，避免在日志中泄漏敏感数据；如需深入诊断，应由上层模块负责序列化。
pub enum PipelineMessage {
    /// L4/L5 字节缓冲。
    Buffer(Box<dyn ReadableBuffer>),
    /// L7 业务消息。
    User(Box<dyn Any + Send + Sync>),
}

/// 辅助类型：轻量快照的字节容器，适用于测试或日志导出。
pub type Bytes = Vec<u8>;

impl fmt::Debug for PipelineMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineMessage::Buffer(_) => {
                f.debug_tuple("Buffer").field(&"<erased-buffer>").finish()
            }
            PipelineMessage::User(_) => f.debug_tuple("User").field(&"<erased-user>").finish(),
        }
    }
}
