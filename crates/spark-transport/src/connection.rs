use alloc::borrow::Cow;
use core::future::Future;

use crate::{BackpressureDecision, BackpressureMetrics, ShutdownDirection, TransportSocketAddr};
use bytes::{Buf, BufMut};

/// 统一的传输连接接口。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 为 TCP/QUIC/TLS 等字节流协议提供一致的读写/关闭接口；
/// - 将背压探测、预算消耗与上下文传递纳入统一契约，便于上层调度器复用同一流程；
/// - 允许在运行期热替换不同传输实现，而无需修改业务代码。
///
/// ## 架构定位（Architecture）
/// - 该 trait 位于 `spark-transport` 核心层，供各实现 crate (`spark-transport-tcp` 等) 实现；
/// - 上层可通过泛型或 trait object (`dyn TransportConnection`) 统一调度不同协议的连接。
///
/// ## 契约说明（What）
/// - `CallCtx<'ctx>`：读写操作使用的上下文类型（如 `CallContext`）。
/// - `ReadyCtx<'ctx>`：背压/就绪检查使用的上下文类型（如 `Context<'ctx>`）。
/// - `read`/`write`/`shutdown` 返回 `Future`，继承上下文的取消与预算；
/// - `classify_backpressure` 将内部状态转换为 [`BackpressureDecision`]；
/// - `peer_addr`/`local_addr` 返回结构化地址，若实现无该信息可返回 `None`。
///
/// ## 风险提示（Trade-offs）
/// - 建议实现保持方法非阻塞，并在 `Future` 内自行处理超时/取消；
/// - 若协议不支持半关闭，应在 `shutdown` 中返回错误或记录限制；
/// - `classify_backpressure` 应避免昂贵计算，可利用缓存或轻量指标。
pub trait TransportConnection: Send + Sync + 'static {
    /// 协议特定的错误类型。
    type Error: core::fmt::Debug + Send + Sync + 'static;
    /// 读写时使用的上下文类型。
    type CallCtx<'ctx>: ?Sized;
    /// 背压检查使用的上下文类型。
    type ReadyCtx<'ctx>: ?Sized;

    /// 读操作返回的 Future。
    type ReadFuture<'ctx>: Future<Output = crate::Result<usize, Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 写操作返回的 Future。
    type WriteFuture<'ctx>: Future<Output = crate::Result<usize, Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 半关闭操作返回的 Future。
    type ShutdownFuture<'ctx>: Future<Output = crate::Result<(), Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 刷新操作返回的 Future。
    type FlushFuture<'ctx>: Future<Output = crate::Result<(), Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 返回可用于日志或追踪的连接 ID。
    fn id(&self) -> Cow<'_, str>;

    /// 读取对端地址。
    fn peer_addr(&self) -> Option<TransportSocketAddr>;

    /// 读取本地地址。
    fn local_addr(&self) -> Option<TransportSocketAddr>;

    /// 读取数据到缓冲区。
    fn read<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        buf: &'ctx mut (dyn BufMut + Send + Sync + 'static),
    ) -> Self::ReadFuture<'ctx>;

    /// 写入数据。
    fn write<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        buf: &'ctx mut (dyn Buf + Send + Sync + 'static),
    ) -> Self::WriteFuture<'ctx>;

    /// 刷新缓冲区。
    fn flush<'ctx>(&'ctx self, ctx: &'ctx Self::CallCtx<'ctx>) -> Self::FlushFuture<'ctx>;

    /// 执行半关闭。
    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        direction: ShutdownDirection,
    ) -> Self::ShutdownFuture<'ctx>;

    /// 基于内部状态给出背压决策。
    fn classify_backpressure(
        &self,
        ctx: &Self::ReadyCtx<'_>,
        metrics: &BackpressureMetrics,
    ) -> BackpressureDecision;
}

/// 面向无连接协议（UDP/QUIC Datagram）的端点契约。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 为无连接传输提供与字节流并行的统一接口，支撑 SIP、DNS 等报文型协议；
/// - 保留上下文传递能力，使限流/取消语义能够覆盖到报文收发。
///
/// ## 契约说明（What）
/// - `CallCtx<'ctx>`：收发时使用的上下文类型；
/// - `recv`：读取单个报文并返回实际长度与对端地址；
/// - `send`：向指定地址发送报文，成功返回写入字节数；
/// - `local_addr`：查询绑定地址。
///
/// ## 风险提示（Trade-offs）
/// - 实现应注意重入安全，避免在 `recv` 尚未完成时重复调用导致竞态；
/// - 若协议需要追踪更多元数据，可在返回值中附加结构体而非仅返回长度。
pub trait DatagramEndpoint: Send + Sync + 'static {
    /// 协议特定的错误类型。
    type Error: core::fmt::Debug + Send + Sync + 'static;
    /// 收发使用的上下文类型。
    type CallCtx<'ctx>: ?Sized;
    /// 入站报文附带的元数据（如对端地址、握手参数）。
    type InboundMeta;
    /// 出站报文所需的元数据（如返回路径、拥塞窗口标签）。
    type OutboundMeta: ?Sized;

    /// 接收报文返回的 Future。
    type RecvFuture<'ctx>: Future<Output = crate::Result<(usize, Self::InboundMeta), Self::Error>>
        + Send
        + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 发送报文返回的 Future。
    type SendFuture<'ctx>: Future<Output = crate::Result<usize, Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    /// 查询本地绑定地址。
    fn local_addr(&self) -> crate::Result<TransportSocketAddr, Self::Error>;

    /// 接收单个报文并返回长度与对端地址。
    fn recv<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        buf: &'ctx mut [u8],
    ) -> Self::RecvFuture<'ctx>;

    /// 向指定对端发送报文。
    fn send<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        payload: &'ctx [u8],
        meta: &'ctx Self::OutboundMeta,
    ) -> Self::SendFuture<'ctx>;
}
