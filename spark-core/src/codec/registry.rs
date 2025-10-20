use super::decoder::{DecodeContext, DecodeOutcome, Decoder as DecoderTrait};
use super::encoder::{EncodeContext, EncodedPayload, Encoder};
use super::metadata::{CodecDescriptor, ContentEncoding, ContentType};
use crate::buffer::ErasedSparkBuf;
use crate::error::codes;
use crate::{CoreError, sealed::Sealed};
use alloc::{boxed::Box, format, vec::Vec};
use core::{any::Any, marker::PhantomData};

/// `Codec` 同时封装编码与解码能力，适合静态泛型环境使用。
///
/// # 设计背景（Why）
/// - 参考 Rust `tokio-util::codec::Decoder + Encoder` 组合模式，以单一 trait 同时表达双向能力，减少泛型参数数量。
/// - 在无状态实现占主导的通信框架中，通过关联类型显式区分入站与出站对象。
///
/// # 逻辑解析（How）
/// - `descriptor` 返回统一元信息；
/// - `encode`、`decode` 分别调用底层序列化/反序列化；
/// - 默认实现 `Encoder`、`Decoder`，方便在泛型上下文中复用。
///
/// # 契约说明（What）
/// - **前置条件**：实现者需保证入站、出站对象符合 `Send + Sync + 'static`，以支持跨线程传输；
/// - **后置条件**：每次调用必须遵循 `EncodeContext`、`DecodeContext` 的预算约束，否则可能触发上层的背压或熔断策略。
///
/// # 风险提示（Trade-offs）
/// - Trait 本身未规定状态同步机制；若实现内部存储会话数据，应自行保证线程安全。
pub trait Codec: Send + Sync + 'static + Sealed {
    /// 解码后的业务类型。
    type Incoming: Send + Sync + 'static;
    /// 编码时的业务类型。
    type Outgoing: Send + Sync + 'static;

    /// 返回该编解码器的描述符。
    fn descriptor(&self) -> &CodecDescriptor;

    /// 编码出站对象。
    fn encode(
        &self,
        item: &Self::Outgoing,
        ctx: &mut EncodeContext<'_>,
    ) -> Result<EncodedPayload, CoreError>;

    /// 解码入站字节。
    fn decode(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> Result<DecodeOutcome<Self::Incoming>, CoreError>;
}

impl<T> Encoder for T
where
    T: Codec,
{
    type Item = T::Outgoing;

    fn descriptor(&self) -> &CodecDescriptor {
        Codec::descriptor(self)
    }

    fn encode(
        &self,
        item: &Self::Item,
        ctx: &mut EncodeContext<'_>,
    ) -> Result<EncodedPayload, CoreError> {
        Codec::encode(self, item, ctx)
    }
}

impl<T> DecoderTrait for T
where
    T: Codec,
{
    type Item = T::Incoming;

    fn descriptor(&self) -> &CodecDescriptor {
        Codec::descriptor(self)
    }

    fn decode(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> Result<DecodeOutcome<Self::Item>, CoreError> {
        Codec::decode(self, src, ctx)
    }
}

/// `DynCodec` 为注册中心提供对象安全的编解码接口。
///
/// # 设计背景（Why）
/// - 在多协议支持场景（如 HTTP/1.1 + HTTP/2、Protobuf + JSON）下，需要将不同实现以 trait 对象形式存放。
/// - 借鉴 Netty `ChannelHandler` 与 Tower `Service` 的 type-erasure 经验，通过 `Any` 做运行时类型检查。
///
/// # 逻辑解析（How）
/// - `encode_dyn` 接收 `Any` 引用并尝试下转型为具体业务类型。
/// - `decode_dyn` 将解码结果装箱为 `Box<dyn Any + Send + Sync>`，供上层根据约定还原类型。
///
/// # 契约说明（What）
/// - **前置条件**：调用方必须确保传入的 `Any` 与编解码器的 `Outgoing` 类型一致。
/// - **后置条件**：若类型不匹配，将返回 `CoreError::new(codes::PROTOCOL_TYPE_MISMATCH, ..)`，调用方需捕获并记录。
///
/// # 风险提示（Trade-offs）
/// - 类型擦除带来运行时成本；在性能敏感路径应优先使用静态泛型 `Codec`。
pub trait DynCodec: Send + Sync + 'static + Sealed {
    /// 获取描述符。
    fn descriptor(&self) -> &CodecDescriptor;

    /// 对象安全的编码入口。
    fn encode_dyn(
        &self,
        item: &(dyn Any + Send + Sync),
        ctx: &mut EncodeContext<'_>,
    ) -> Result<EncodedPayload, CoreError>;

    /// 对象安全的解码入口。
    fn decode_dyn(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> Result<DecodeOutcome<Box<dyn Any + Send + Sync>>, CoreError>;
}

/// `TypedCodecAdapter` 将静态泛型 `Codec` 包装为对象安全的 `DynCodec`。
///
/// # 设计背景（Why）
/// - Netty 的 `MessageToMessageCodec` 与 Tower 的 `Layer` 常通过适配器桥接泛型与 trait 对象，本类型延续该模式。
/// - 便于在注册中心内存放多个不同实现，同时保持调用端的统一接口。
///
/// # 逻辑解析（How）
/// - `new` 持有内部实现；
/// - `encode_dyn` 使用 `Any::downcast_ref` 还原类型；
/// - `decode_dyn` 将结果打包为 `Box<dyn Any>`，供调用方再进行类型匹配。
///
/// # 契约说明（What）
/// - **前置条件**：调用 `encode_dyn` 的上层必须确保传入对象类型正确，否则会触发类型不匹配错误。
/// - **后置条件**：适配器不缓存状态，对象生命周期与内部 `Codec` 一致。
///
/// # 风险提示（Trade-offs）
/// - 运行时下转型会引入微小开销；若热路径中频繁调用，建议在更外层缓存 `TypeId`，或直接使用泛型接口。
pub struct TypedCodecAdapter<C>
where
    C: Codec,
{
    inner: C,
}

impl<C> TypedCodecAdapter<C>
where
    C: Codec,
{
    /// 构建新的适配器。
    pub fn new(inner: C) -> Self {
        Self { inner }
    }

    /// 取出内部的实现。
    pub fn into_inner(self) -> C {
        self.inner
    }
}

impl<C> DynCodec for TypedCodecAdapter<C>
where
    C: Codec,
{
    fn descriptor(&self) -> &CodecDescriptor {
        self.inner.descriptor()
    }

    fn encode_dyn(
        &self,
        item: &(dyn Any + Send + Sync),
        ctx: &mut EncodeContext<'_>,
    ) -> Result<EncodedPayload, CoreError> {
        match item.downcast_ref::<C::Outgoing>() {
            Some(typed) => self.inner.encode(typed, ctx),
            None => Err(CoreError::new(
                codes::PROTOCOL_TYPE_MISMATCH,
                format!(
                    "期待类型 `{}`，实际收到不兼容类型",
                    core::any::type_name::<C::Outgoing>(),
                ),
            )),
        }
    }

    fn decode_dyn(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> Result<DecodeOutcome<Box<dyn Any + Send + Sync>>, CoreError> {
        match self.inner.decode(src, ctx)? {
            DecodeOutcome::Complete(item) => Ok(DecodeOutcome::Complete(Box::new(item))),
            DecodeOutcome::Incomplete => Ok(DecodeOutcome::Incomplete),
            DecodeOutcome::Skipped => Ok(DecodeOutcome::Skipped),
        }
    }
}

/// `NegotiatedCodec` 描述一次内容协商的结果。
///
/// # 设计背景（Why）
/// - 参考 HTTP `Accept`/`Content-Type` 与 gRPC 的 `content-subtype` 协议，握手阶段需记录最终选择及备选方案。
/// - 保留兜底编码列表，便于在传输层降级或回退。
///
/// # 逻辑解析（How）
/// - `new` 以最终描述符初始化；
/// - `with_fallback_encodings` 记录优先级排序的备选编码；
/// - `descriptor`、`fallback_encodings` 分别返回最终方案与备选列表。
///
/// # 契约说明（What）
/// - **前置条件**：列表中的编码需按优先级排列；
/// - **后置条件**：结构体可安全克隆并跨线程共享。
///
/// # 风险提示（Trade-offs）
/// - 未存储权重（如 HTTP `q` 参数）；若需要，请在上层扩展结构。
#[derive(Clone, Debug)]
pub struct NegotiatedCodec {
    descriptor: CodecDescriptor,
    fallback_encodings: Vec<ContentEncoding>,
}

impl NegotiatedCodec {
    /// 创建新的协商结果。
    pub fn new(descriptor: CodecDescriptor) -> Self {
        Self {
            descriptor,
            fallback_encodings: Vec::new(),
        }
    }

    /// 设置备选编码列表。
    pub fn with_fallback_encodings(mut self, fallback: Vec<ContentEncoding>) -> Self {
        self.fallback_encodings = fallback;
        self
    }

    /// 获取最终协商的描述符。
    pub fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    /// 返回按优先级排序的备选编码列表。
    pub fn fallback_encodings(&self) -> &[ContentEncoding] {
        &self.fallback_encodings
    }
}

/// `DynCodecFactory` 定义对象安全的编解码实例创建契约。
///
/// # 设计背景（Why）
/// - 对标 Netty `ChannelInitializer` 与 gRPC `CodecFactory`，允许运行时按需生成编解码实例。
/// - 在多租户环境下，可将协商参数传入工厂，定制每条连接的细节（如 schema 版本）。
///
/// # 逻辑解析（How）
/// - `descriptor` 返回工厂支持的描述符；
/// - `instantiate` 根据协商结果创建新的编解码实例。
///
/// # 契约说明（What）
/// - **前置条件**：传入的 `NegotiatedCodec` 必须与工厂描述符兼容，否则应返回 `CoreError::new(codes::PROTOCOL_NEGOTIATION, ..)` 等错误；
/// - **后置条件**：成功返回的对象必须独立、线程安全，允许并发使用。
///
/// # 风险提示（Trade-offs）
/// - 工厂可能缓存状态以复用对象，此时需考虑生命周期与共享所有权问题。
pub trait DynCodecFactory: Send + Sync + 'static + Sealed {
    /// 获取工厂支持的描述符。
    fn descriptor(&self) -> &CodecDescriptor;

    /// 基于协商结果构建编解码实例。
    fn instantiate(&self, negotiated: &NegotiatedCodec) -> Result<Box<dyn DynCodec>, CoreError>;
}

/// `TypedCodecFactory` 将返回具体 `Codec` 的构造器包装为 `DynCodecFactory`。
///
/// # 设计背景（Why）
/// - 在静态语言中实现注册中心时，常以闭包/函数指针生成编解码实例，本结构体将其转换为对象安全接口。
///
/// # 逻辑解析（How）
/// - 内部持有描述符与构造闭包；
/// - `instantiate` 调用闭包并使用 `TypedCodecAdapter` 包装结果。
///
/// # 契约说明（What）
/// - **前置条件**：闭包必须返回符合描述符声明的 `Codec`；
/// - **后置条件**：返回的对象安全实例可立即投入使用。
///
/// # 风险提示（Trade-offs）
/// - 若闭包捕获外部状态，请确保其 `Send + Sync`，否则将违背 trait 要求。
pub struct TypedCodecFactory<C, F>
where
    C: Codec,
    F: Fn(&NegotiatedCodec) -> C + Send + Sync + 'static,
{
    descriptor: CodecDescriptor,
    constructor: F,
    _marker: PhantomData<C>,
}

impl<C, F> TypedCodecFactory<C, F>
where
    C: Codec,
    F: Fn(&NegotiatedCodec) -> C + Send + Sync + 'static,
{
    /// 使用描述符与构造函数创建工厂。
    pub fn new(descriptor: CodecDescriptor, constructor: F) -> Self {
        Self {
            descriptor,
            constructor,
            _marker: PhantomData,
        }
    }
}

impl<C, F> DynCodecFactory for TypedCodecFactory<C, F>
where
    C: Codec,
    F: Fn(&NegotiatedCodec) -> C + Send + Sync + 'static,
{
    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn instantiate(&self, negotiated: &NegotiatedCodec) -> Result<Box<dyn DynCodec>, CoreError> {
        let codec = (self.constructor)(negotiated);
        Ok(Box::new(TypedCodecAdapter::new(codec)))
    }
}

/// `CodecRegistry` 约束编解码注册中心的核心能力。
///
/// # 设计背景（Why）
/// - 在跨平台通信框架中，客户端通常通过 `Accept` 列表声明可理解的内容类型，服务端需要根据优先级选取最优方案。
/// - 该接口抽象出三项职责：注册、协商、实例化，以便在不同存储（内存表、分布式配置中心）间复用。
///
/// # 逻辑解析（How）
/// - `register` 将新的工厂加入注册表，可能涉及去重或覆盖策略；
/// - `negotiate` 接收客户端偏好列表并返回服务端选择；
/// - `instantiate` 根据协商结果构建实例，保持解耦。
///
/// # 契约说明（What）
/// - **前置条件**：`preferred` 与 `encodings` 应按照客户端优先级排序；
/// - **后置条件**：协商成功后返回的 `NegotiatedCodec` 应被视为只读快照，可在多线程间共享。
///
/// # 风险提示（Trade-offs）
/// - 注册中心可能成为共享状态热点，实现时需注意并发控制与读写锁策略；
/// - 协商失败时必须返回带有稳定错误码的 `CoreError`，以便客户端快速定位问题。
pub trait CodecRegistry: Send + Sync + 'static + Sealed {
    /// 注册新的编解码工厂。
    fn register(&self, factory: Box<dyn DynCodecFactory>) -> Result<(), CoreError>;

    /// 根据客户端偏好协商编解码方案。
    fn negotiate(
        &self,
        preferred: &[ContentType],
        accepted_encodings: &[ContentEncoding],
    ) -> Result<NegotiatedCodec, CoreError>;

    /// 基于协商结果创建编解码实例。
    fn instantiate(&self, negotiated: &NegotiatedCodec) -> Result<Box<dyn DynCodec>, CoreError>;
}
