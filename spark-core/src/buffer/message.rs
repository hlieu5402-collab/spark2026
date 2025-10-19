use super::ReadableBuffer;
use alloc::{boxed::Box, vec::Vec};
use core::{any::Any, fmt};

/// `UserMessage` 描述所有可放入 [`PipelineMessage::User`] 的业务消息契约。
///
/// # 设计背景（Why）
/// - 早期版本直接使用 `Box<dyn Any + Send + Sync>`，虽然灵活但缺乏静态约束，调用方在运行时下转型时容易遗漏判错逻辑。
/// - 本 Trait 为对象安全抽象，借助统一的 `message_kind` 与 `as_any` 系列方法，在保持扩展性的同时提升类型自解释能力。
/// - 设计与 Rust `TypeId` 思路一致，允许通过 `type_name` 暴露稳定的诊断字符串，便于可观测性与调试工具识别消息类别。
///
/// # 逻辑解析（How）
/// - `message_kind` 默认返回编译期类型名，便于在日志或指标中识别消息类别。
/// - `as_any` / `as_any_mut` / `into_any` 为运行时安全下转型提供统一入口，避免重复编写 `Any` 强制转换样板代码。
/// - 采用对象安全方法签名，确保可放入 `Box<dyn UserMessage>` 并跨线程传递。对于满足 `Send + Sync + 'static` 的类型，crate 默认提供 blanket 实现。
///
/// # 扩展契约评估（Marker Trait）
/// - 经过评估，`UserMessage` 本身即作为显式标记 Trait，替代了早期的 `Box<dyn Any>` 方案，在保持开放扩展的同时提供静态语义。
/// - 若未来需要附加约束（如统一序列化格式或鉴权标签），可在该 Trait 上新增关联常量/方法，而无需回退到裸 `Any`。
/// - `Any` 相关接口保留用于运行时类型识别，建议搭配 [`PipelineMessage::downcast_user_ref`] 等工具函数统一处理失败分支，避免遗漏判错。
///
/// # 契约说明（What）
/// - **输入**：实现者必须保证类型满足 `Send + Sync + 'static`，以便跨线程及长期存活场景安全复用。
/// - **输出**：调用 `as_any*` 返回的引用/Box 仅用于类型判定，不得在外部强行转换为错误类型，否则会触发 panic。
/// - **前置条件**：调用 `into_any` 前需要确认后续确实要进行类型判定，否则建议直接使用 `as_any` 避免不必要的堆所有权转移。
/// - **后置条件**：`into_any` 返回的 `Box<dyn Any>` 可以继续使用标准库 `downcast` 完成强类型提取。
///
/// # 风险与权衡（Trade-offs）
/// - 引入 Trait 会在极端热路径上带来一次虚函数分发，但 `async_contract_overhead` 基准测试显示该成本远低于消息序列化与网络开销。
/// - 若业务类型需要克隆，可结合 `Arc` 或在 Trait 扩展方法中引入 `clone_box`，本抽象刻意保持最小化以免约束实现者。
pub trait UserMessage: Send + Sync + 'static {
    /// 返回消息类别标识，默认使用编译期类型名。
    fn message_kind(&self) -> &'static str {
        core::any::type_name::<Self>()
    }

    /// 以 `Any` 引用形式暴露自身，供调用方进行 `downcast_ref`。
    fn as_any(&self) -> &dyn Any;

    /// 暴露可变引用版本的 `Any`，便于在 Handler 中原地修改消息。
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// 将消息转移为 `Box<dyn Any>`，以匹配标准库的所有权下转型 API。
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

impl<T> UserMessage for T
where
    T: Any + Send + Sync + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// `PipelineMessage` 统一承载网络层字节与业务层对象。
///
/// # 设计背景（Why）
/// - 借鉴 Netty `ChannelPipeline`、Akka Stream `ByteString`、NATS `Message`、gRPC `Message`、Apache Pulsar `Message` 的复合消息模式，确保在一个通道内安全穿梭不同层级的数据。
/// - 框架内部需要在协议编解码、负载均衡、服务治理之间传递异构数据，因此需通过 trait 对象屏蔽具体类型。
///
/// # 逻辑解析（How）
/// - `Buffer` 变体封装 [`ReadableBuffer`]，用于承载 L4/L5 字节流，适配零拷贝与池化策略。
/// - `User` 变体封装任意实现 [`UserMessage`] 的对象，对应 L7 业务语义；通过 `UserMessage::as_any` 支持运行时下转型且提供类型标签。
/// - `Bytes` 类型别名提供轻量级 `Vec<u8>` 快照，方便边缘组件或测试用例无需实现 `ReadableBuffer` 也能消费数据。
///
/// # `Any` 使用指南
/// - `UserMessage` 通过 `as_any`/`into_any` 暴露 `Any` 接口，仅用于运行时类型识别，避免直接操作裸 `Box<dyn Any>`。
/// - 推荐调用 [`PipelineMessage::downcast_user_ref`]、[`PipelineMessage::downcast_user_mut`] 与 [`PipelineMessage::try_into_user`] 进行安全转换，并在失败时结合 [`PipelineMessage::user_kind`] 记录诊断信息。
/// - 若需要更强的编译期约束，可在业务模块中定义更细粒度的消息 Trait，并统一实现 `UserMessage` 以复用对象安全能力。
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
///
/// # 示例（Example）
/// ```
/// use spark_core::buffer::PipelineMessage;
///
/// #[derive(Debug)]
/// struct LoginEvent;
///
/// // `LoginEvent` 满足 `Send + Sync + 'static`，自动实现 `UserMessage`。
/// let mut message = PipelineMessage::from_user(LoginEvent);
/// assert!(message.downcast_user_ref::<LoginEvent>().is_some());
/// assert_eq!(
///     message.user_kind(),
///     Some(core::any::type_name::<LoginEvent>())
/// );
/// let extracted = message.try_into_user::<LoginEvent>().expect("owns value");
/// drop(extracted);
/// ```
pub enum PipelineMessage {
    /// L4/L5 字节缓冲。
    Buffer(Box<dyn ReadableBuffer>),
    /// L7 业务消息。
    User(Box<dyn UserMessage>),
}

impl PipelineMessage {
    /// 构造 `User` 变体的便捷函数。
    ///
    /// # 设计背景（Why）
    /// - 与 `PipelineMessage::User(Box::new(msg))` 相比，该方法统一封装 `UserMessage` 约束，减少调用方遗漏 `Send + Sync` 检查的风险。
    /// - 同时在注释中明确输入/输出契约，方便初次接入者理解如何安全扩展 Pipeline。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：`message` 为实现 [`UserMessage`] 的业务载荷，通常为结构体或枚举，需满足线程安全语义。
    /// - **返回值**：包装后的 [`PipelineMessage::User`]，可直接交由 Pipeline Handler 传递。
    /// - **前置条件**：调用方应确保消息不会在 Handler 间产生共享可变状态；如需共享请使用 `Arc` 包裹。
    /// - **后置条件**：返回值内部持有堆分配对象，生命周期由 Pipeline 管理，调用方无需额外释放。
    pub fn from_user<M>(message: M) -> Self
    where
        M: UserMessage,
    {
        Self::User(Box::new(message))
    }

    /// 查询 `User` 变体的消息类别标识。
    ///
    /// # 使用说明
    /// - 若当前不是 `User` 变体，返回 `None`。
    /// - 该标识基于 [`UserMessage::message_kind`]，适合在日志或指标中记录，用于排查类型转换失败原因。
    pub fn user_kind(&self) -> Option<&'static str> {
        match self {
            PipelineMessage::User(msg) => Some(msg.message_kind()),
            _ => None,
        }
    }

    /// 尝试以不可变引用形式将 `User` 变体下转型为具体类型。
    ///
    /// # 返回值语义
    /// - 匹配成功返回 `Some(&T)`；
    /// - 若类型不符或非 `User` 变体，返回 `None`，调用方应结合日志记录 `user_kind` 进行诊断。
    pub fn downcast_user_ref<T>(&self) -> Option<&T>
    where
        T: UserMessage,
    {
        match self {
            PipelineMessage::User(msg) => msg.as_any().downcast_ref::<T>(),
            _ => None,
        }
    }

    /// 尝试以可变引用形式将 `User` 变体下转型为具体类型。
    ///
    /// # 使用场景
    /// - 在 Handler 中需要原地修改业务载荷时可调用该方法。
    /// - 调用者必须保证修改后的状态仍满足实现 [`UserMessage`] 时的约束（如字段不违反业务不变量）。
    pub fn downcast_user_mut<T>(&mut self) -> Option<&mut T>
    where
        T: UserMessage,
    {
        match self {
            PipelineMessage::User(msg) => msg.as_any_mut().downcast_mut::<T>(),
            _ => None,
        }
    }

    /// 以所有权方式尝试提取具体业务消息。
    ///
    /// # 返回值契约
    /// - 成功时返回 `Ok(T)`，调用者即可获得具体类型的所有权继续处理；
    /// - 失败时返回原封不动的 `PipelineMessage`，确保不会丢失消息，便于调用方走兜底逻辑（例如记录错误后回退到泛型处理）。
    pub fn try_into_user<T>(self) -> Result<T, Self>
    where
        T: UserMessage,
    {
        match self {
            PipelineMessage::User(msg) => {
                if msg.as_any().is::<T>() {
                    let boxed = msg
                        .into_any()
                        .downcast::<T>()
                        .expect("type check guarantees downcast success");
                    Ok(*boxed)
                } else {
                    Err(PipelineMessage::User(msg))
                }
            }
            other => Err(other),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 业务消息示例，验证 `UserMessage` 下转型工具函数的行为。
    #[derive(Debug, PartialEq, Eq)]
    struct SampleUserMessage {
        marker: u8,
    }

    #[test]
    fn downcast_helpers_work_for_user_message() {
        let mut message = PipelineMessage::from_user(SampleUserMessage { marker: 7 });
        assert_eq!(
            message.user_kind(),
            Some(core::any::type_name::<SampleUserMessage>())
        );
        assert!(message.downcast_user_ref::<SampleUserMessage>().is_some());
        if let Some(sample) = message.downcast_user_mut::<SampleUserMessage>() {
            sample.marker = 9;
        }
        let extracted = message
            .try_into_user::<SampleUserMessage>()
            .expect("owned downcast");
        assert_eq!(extracted.marker, 9);
    }

    #[test]
    fn try_into_user_preserves_original_on_failure() {
        let message = PipelineMessage::from_user(SampleUserMessage { marker: 1 });
        let recovered = match message.try_into_user::<u32>() {
            Ok(_) => panic!("downcast should fail"),
            Err(original) => original,
        };
        assert!(recovered.downcast_user_ref::<SampleUserMessage>().is_some());
    }
}
