use crate::{CoreError, sealed::Sealed};
use alloc::boxed::Box;
use core::mem::MaybeUninit;

use super::ReadableBuffer;

/// `WritableBuffer` 描述统一的可写缓冲契约。
///
/// # 设计背景（Why）
/// - **行业借鉴**：融合 Tokio `bytes::BufMut`、Netty `CompositeByteBuf`、Envoy `Buffer::Instance`、Apache Arrow `BufferBuilder`、.NET `PipeWriter` 的写入语义，确保兼容多种序列化/协议栈需求。
/// - **框架职责**：运行时、协议编解码、分布式聚合都需要在共享缓冲上高频写入，契约必须清晰划定扩容、写入、冻结的边界。
/// - **前沿趋势**：面向 io-uring、DPDK、eBPF 用户态网络等场景，需要支持池化、零拷贝以及“写后即读”的冻结转换。
///
/// # 逻辑解析（How）
/// - `reserve` 借鉴 PipeWriter 的“增量扩容”模式，鼓励实现按需增长并向调用方反馈失败。
/// - `put_slice`/`write_from` 提供从切片及 `ReadableBuffer` 的双通道写入，适配传统序列化（JSON/Protobuf）与零拷贝转发。
/// - `freeze` 参考 Tokio Bytes 的 `freeze` 语义，将可写缓冲转换为 `ReadableBuffer`，确保所有权安全转移。
/// - `clear` 对标 Netty 中的 `clear()`，便于重复使用池内缓冲。
///
/// # 契约说明（What）
/// - **输入参数**：
///   - `reserve(additional)` 表示最少需要追加的可写空间。
///   - `put_slice(src)` 会写入 `src` 全部字节；`write_from(src, len)` 则会从 `src` 读取 `len` 字节。
/// - **返回值**：
///   - 写入、扩容与冻结操作成功时返回 `Ok(())` 或 `Ok(Box<dyn ReadableBuffer>)`；失败时应附带稳定错误码。
/// - **前置条件**：调用方需遵循顺序写入，不允许并发写入同一实例；跨线程共享需借助外部同步原语。
/// - **后置条件**：成功写入后 `written()` 必须立即可见；`freeze` 之后原对象不可再写。
///
/// # 设计考量（Trade-offs & Gotchas）
/// - **容量模型**：采用“剩余容量”与“已写入”双指标，避免像传统 `Vec` 那样需要额外追踪长度。
/// - **错误语义**：保持 `Result` 返回值，使实现可以在内存池耗尽、写入受限（背压）时显式失败。
/// - **性能提示**：`write_from` 建议通过内存拷贝加速指令或零拷贝（引用计数）实现，以降低跨缓冲搬运成本。
/// - **冻结风险**：冻结后若底层仍被共享，必须确保引用计数正确递增，防止悬垂引用。
pub trait WritableBuffer: Send + Sync + 'static + Sealed {
    /// 总容量上限（包含已写入字节）。
    fn capacity(&self) -> usize;

    /// 剩余可写空间。
    fn remaining_mut(&self) -> usize;

    /// 已写入的字节数，便于观测与回收策略。
    fn written(&self) -> usize;

    /// 确保至少追加 `additional` 字节的可写空间。
    fn reserve(&mut self, additional: usize) -> crate::Result<(), CoreError>;

    /// 将切片写入缓冲末尾。
    fn put_slice(&mut self, src: &[u8]) -> crate::Result<(), CoreError>;

    /// 从 `ReadableBuffer` 转写 `len` 字节。
    fn write_from(
        &mut self,
        src: &mut dyn ReadableBuffer,
        len: usize,
    ) -> crate::Result<(), CoreError>;

    /// 暴露当前可写入的连续内存切片，返回值使用 [`MaybeUninit`] 表示尚未初始化的字节。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：
    ///   - 传输层在读取底层 socket 时希望直接写入缓冲区，避免先读到临时 `Vec<u8>` 再调用
    ///     [`put_slice`](Self::put_slice) 造成的额外拷贝；
    ///   - 模仿 `bytes::BufMut::chunk_mut` 语义，让零拷贝契约覆盖 `codec-*` 与 `transport-*` 的读路径。
    /// - **实现要点 (How)**：
    ///   - 返回的切片必须指向尚未初始化但可安全写入的连续内存；
    ///   - 调用方在写入后需配合 [`advance_mut`](Self::advance_mut) 宣告实际写入字节数；
    ///   - 若当前缓冲不可写（例如已被冻结），应返回长度为 0 的切片。
    /// - **契约说明 (What)**：
    ///   - **前置条件**：实现者需确保在返回切片期间不会触发重新分配或缩容，避免悬垂指针；
    ///   - **后置条件**：切片生命周期与 `&mut self` 等长，调用方不得保留超过本次借用的引用；
    ///   - **错误防御**：切片长度应与 [`remaining_mut`](Self::remaining_mut) 一致或为其下界，若长度为 0
    ///     则视为没有可写空间。
    /// - **风险提示 (Trade-offs)**：
    ///   - 某些实现（如通过共享内存映射的缓冲）可能需要显式“固定”内存以满足此接口；
    ///   - 若实现选择返回缓存池中的大块区域，应权衡碎片化与写放大风险。
    fn chunk_mut(&mut self) -> &mut [MaybeUninit<u8>];

    /// 标记最近一次通过 [`chunk_mut`](Self::chunk_mut) 写入的字节数。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：
    ///   - 与 `chunk_mut` 配套，显式告知缓冲实现“已有多少字节被初始化”，以便更新内部写入指针；
    ///   - 避免在传输层调用 `put_slice` 时再次复制数据，实现真正的零拷贝写入路径。
    /// - **执行步骤 (How)**：
    ///   1. 调用方在获得切片后，将底层 IO 的读取结果写入该切片；
    ///   2. 按实际写入的 `len` 调用本方法；
    ///   3. 实现需校验 `len <= chunk_mut().len()`，并推进内部游标或更新长度。
    /// - **契约说明 (What)**：
    ///   - **输入**：`len` 表示本次新增的有效字节数；
    ///   - **前置条件**：必须先调用 [`chunk_mut`](Self::chunk_mut) 获取可写区域；
    ///   - **后置条件**：成功后 [`written`](Self::written) 增加 `len`，并保证后续 `freeze` 能够观察到新数据。
    /// - **风险提示 (Trade-offs)**：
    ///   - 若调用方传入超出可写空间的长度，实现应返回 [`CoreError`] 并保持内部状态不变；
    ///   - 对于引用计数或分片缓冲，实现者需确保推进写指针不会破坏其它别名引用的语义。
    fn advance_mut(&mut self, len: usize) -> crate::Result<(), CoreError>;

    /// 清空已写内容但保留容量，便于重复使用。
    fn clear(&mut self);

    /// 冻结缓冲区，转换为只读视图。
    fn freeze(self: Box<Self>) -> crate::Result<Box<dyn ReadableBuffer>, CoreError>;
}

#[allow(unsafe_code)]
unsafe impl bytes::BufMut for dyn WritableBuffer + Send + Sync + 'static {
    fn remaining_mut(&self) -> usize {
        WritableBuffer::remaining_mut(self)
    }

    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        WritableBuffer::chunk_mut(self).into()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        if cnt == 0 {
            return;
        }
        if let Err(err) = WritableBuffer::advance_mut(self, cnt) {
            panic!("WritableBuffer::advance_mut failed: {:?}", err);
        }
    }

    fn put_slice(&mut self, src: &[u8]) {
        WritableBuffer::put_slice(self, src)
            .unwrap_or_else(|err| panic!("WritableBuffer::put_slice failed: {:?}", err));
    }
}
