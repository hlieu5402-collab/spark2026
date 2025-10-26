use crate::buffer::ErasedSparkBuf;
use crate::codec::decoder::{DecodeContext, DecodeOutcome, Decoder as DecoderTrait};
use crate::codec::encoder::{EncodeContext, EncodedPayload, Encoder};
use crate::{CoreError, sealed::Sealed};

/// `Codec` 统一封装编码与解码逻辑，是泛型层的零成本编解码契约。
///
/// # 设计初衷（Why）
/// - 借鉴 `tokio-util::codec::Decoder + Encoder` 的组合模式，以单一 trait 同时表达双向能力；
/// - 通过关联类型区分入站/出站业务对象，保证静态类型安全；
/// - 作为对象层 [`super::object::DynCodec`] 的泛型基线，支撑 T05“二层 API”在编解码域的等价实现。
///
/// # 行为逻辑（How）
/// 1. `descriptor` 返回实现所支持的内容类型与压缩信息；
/// 2. `encode` 将业务对象序列化为字节缓冲；
/// 3. `decode` 从缓冲中解析业务对象，并返回统一的 [`DecodeOutcome`]。
///
/// # 契约说明（What）
/// - **关联类型**：`Incoming`/`Outgoing` 均需满足 `Send + Sync + 'static`，以支持跨线程传输；
/// - **输入**：`EncodeContext`/`DecodeContext` 携带预算、观测等约束，上层会在超限时触发熔断；
/// - **前置条件**：调用方必须确保入站缓冲遵循协议约定；
/// - **后置条件**：若返回错误，必须给出语义化 [`CoreError`] 以供上层记录。
///
/// # 风险提示（Trade-offs）
/// - `Codec` 自身不维持状态同步；若实现需要缓存上下文，请在内部保证并发安全；
/// - 在极端性能场景，可结合 Arena/零拷贝缓冲减少复制，仍保持与对象层兼容。
pub trait Codec: Send + Sync + 'static + Sealed {
    /// 解码后的业务类型。
    type Incoming: Send + Sync + 'static;
    /// 编码时的业务类型。
    type Outgoing: Send + Sync + 'static;

    /// 返回编解码器描述符。
    fn descriptor(&self) -> &super::super::metadata::CodecDescriptor;

    /// 编码业务对象。
    fn encode(
        &self,
        item: &Self::Outgoing,
        ctx: &mut EncodeContext<'_>,
    ) -> crate::Result<EncodedPayload, CoreError>;

    /// 解码字节流。
    fn decode(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> crate::Result<DecodeOutcome<Self::Incoming>, CoreError>;
}

impl<T> Encoder for T
where
    T: Codec,
{
    type Item = T::Outgoing;

    fn descriptor(&self) -> &super::super::metadata::CodecDescriptor {
        Codec::descriptor(self)
    }

    fn encode(
        &self,
        item: &Self::Item,
        ctx: &mut EncodeContext<'_>,
    ) -> crate::Result<EncodedPayload, CoreError> {
        Codec::encode(self, item, ctx)
    }
}

impl<T> DecoderTrait for T
where
    T: Codec,
{
    type Item = T::Incoming;

    fn descriptor(&self) -> &super::super::metadata::CodecDescriptor {
        Codec::descriptor(self)
    }

    fn decode(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> crate::Result<DecodeOutcome<Self::Item>, CoreError> {
        Codec::decode(self, src, ctx)
    }
}
