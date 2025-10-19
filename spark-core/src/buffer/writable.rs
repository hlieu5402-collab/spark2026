use crate::CoreError;
use alloc::boxed::Box;

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
pub trait WritableBuffer: Send + Sync + 'static {
    /// 总容量上限（包含已写入字节）。
    fn capacity(&self) -> usize;

    /// 剩余可写空间。
    fn remaining_mut(&self) -> usize;

    /// 已写入的字节数，便于观测与回收策略。
    fn written(&self) -> usize;

    /// 确保至少追加 `additional` 字节的可写空间。
    fn reserve(&mut self, additional: usize) -> Result<(), CoreError>;

    /// 将切片写入缓冲末尾。
    fn put_slice(&mut self, src: &[u8]) -> Result<(), CoreError>;

    /// 从 `ReadableBuffer` 转写 `len` 字节。
    fn write_from(&mut self, src: &mut dyn ReadableBuffer, len: usize) -> Result<(), CoreError>;

    /// 清空已写内容但保留容量，便于重复使用。
    fn clear(&mut self);

    /// 冻结缓冲区，转换为只读视图。
    fn freeze(self: Box<Self>) -> Result<Box<dyn ReadableBuffer>, CoreError>;
}
