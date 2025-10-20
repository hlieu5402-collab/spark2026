use crate::{CoreError, sealed::Sealed};
use alloc::{boxed::Box, vec::Vec};

/// `ReadableBuffer` 定义了对象安全的只读缓冲契约。
///
/// # 设计背景（Why）
/// - **对标实践**：综合 Tokio `bytes::Buf`、Netty `ByteBuf`、gRPC C++ `Slice`、Akka `ByteString`、.NET `ReadOnlySequence` 五大主流缓冲模型，抽象出通用的读取语义。
/// - **框架定位**：流水线各阶段（传输、协议、业务）都需要统一读取视图，避免在热路径中频繁进行类型转换或复制。
/// - **扩展目标**：允许自定义实现以适配零拷贝、共享内存、RDMA 等前沿场景，同时在 `no_std + alloc` 环境中保持可用。
///
/// # 逻辑解析（How）
/// - 按照“观察-拆分-推进”三段式设计：`chunk` 暴露当前可读块，`split_to` 复制借用语义，`advance` 推进读指针。
/// - `copy_into_slice` 提供兼容传统 API 的降级路径，对标 Akka `ByteString` 的拷贝策略。
/// - `try_into_vec` 则吸收 gRPC `Slice::TryFlatten` 思路，在需要跨 FFI 或磁盘持久化时提供一次性扁平化能力。
///
/// # 契约说明（What）
/// - **输入参数**：
///   - `split_to(len)`/`advance(len)` 的 `len` 以字节计，必须满足 `len <= remaining()`。
///   - `copy_into_slice(dst)` 需要调用方保证 `dst.len() <= remaining()`。
/// - **返回值**：
///   - `split_to` 返回新的 `ReadableBuffer` 实例，拥有拆分区段的所有权。
///   - `try_into_vec` 在实现支持时返回一份扁平化字节；若底层为分片结构，可返回 `CoreError` 表示不支持。
/// - **前置条件**：实现必须保持线程安全或引用计数语义，以满足 `Send + Sync` 要求。
/// - **后置条件**：所有推进或拆分操作结束后，`remaining()` 必须准确反映剩余字节数。
///
/// # 设计考量（Trade-offs & Gotchas）
/// - **对象安全权衡**：参考 Netty 的 `ByteBuf`，我们放弃泛型化零成本抽象，以换取跨组件的动态调度能力。
/// - **零拷贝支持**：`chunk` 允许返回分片视图，便于对接 io-uring、DPDK 等前沿 IO 技术；但实现需注意其生命周期仅在下一次可变访问前有效。
/// - **错误处理**：统一使用 `CoreError`，鼓励实现者在容量不足时返回稳定错误码（如 `protocol.budget_exceeded`）。
/// - **性能提示**：频繁调用 `try_into_vec` 会导致复制，应仅在跨安全边界（如 FFI）时使用。
pub trait ReadableBuffer: Send + Sync + 'static + Sealed {
    /// 返回剩余可读字节数。
    fn remaining(&self) -> usize;

    /// 返回当前可直接读取的连续字节块。
    fn chunk(&self) -> &[u8];

    /// 拆分出前 `len` 字节，返回新的缓冲区实例。
    fn split_to(&mut self, len: usize) -> Result<Box<dyn ReadableBuffer>, CoreError>;

    /// 将读指针前移 `len` 字节，丢弃对应数据。
    fn advance(&mut self, len: usize) -> Result<(), CoreError>;

    /// 将缓冲内容复制到 `dst`，兼容传统基于切片的 API。
    fn copy_into_slice(&mut self, dst: &mut [u8]) -> Result<(), CoreError>;

    /// 尝试将剩余数据扁平化为 `Vec<u8>`，供一次性消费场景使用。
    fn try_into_vec(self: Box<Self>) -> Result<Vec<u8>, CoreError>;

    /// 判断缓冲区是否已读空。
    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}
