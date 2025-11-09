use core::{fmt, future::Future};

use super::{ShutdownDirection, TransportSocketAddr, connection::Channel};
use crate::Result;

/// 传输层服务端通道（`ServerChannel`）接口。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 为各类协议（TCP、TLS、QUIC 等）提供与 Netty `ServerSocketChannel`
///   一致的服务端通道抽象，使上层可以在不感知具体协议的情况下接受连接并执行优雅关闭；
/// - 补充 [`Channel`]，共同覆盖服务端监听通道与客户端通道的双向契约。
///
/// ## 契约（What）
/// - `Error`：结构化错误类型；
/// - `AcceptCtx<'ctx>`：接受新连接时的上下文；
/// - `ShutdownCtx<'ctx>`：执行关闭时的上下文；
/// - `Connection`：接受后返回的连接类型，必须实现 [`Channel`]；
/// - `AcceptFuture`：返回 `(Connection, TransportSocketAddr)` 的 Future；
/// - `ShutdownFuture`：执行优雅关闭；
/// - `scheme()`：返回协议标识；
/// - `local_addr()`：查询服务端通道的监听地址；
/// - `accept()`：接受新连接并附带对端地址；
/// - `shutdown()`：根据方向执行关闭；
/// - **前置条件**：上下文生命周期需覆盖 Future 执行时间；
/// - **后置条件**：Future 成功完成即表示操作符合协议语义。
///
/// ## 解析逻辑（How）
/// - 采用 GAT 约束，允许实现直接返回 `async` 块；
/// - `ShutdownDirection` 指示关闭方向，确保调用者显式选择策略；
/// - 返回的地址使用 [`TransportSocketAddr`] 保持跨协议一致。
///
/// ## 风险提示（Trade-offs）
/// - 某些协议的服务端通道可能无法支持精细的半关闭，此时实现者应记录日志并返回适当错误；
/// - Trait 要求 `Send + Sync + 'static`，若运行在受限环境需增加适配层。
pub trait ServerChannel: Send + Sync + 'static {
    type Error: fmt::Debug + Send + Sync + 'static;
    type AcceptCtx<'ctx>;
    type ShutdownCtx<'ctx>;
    type Connection: Channel;

    type AcceptFuture<'ctx>: Future<Output = Result<(Self::Connection, TransportSocketAddr), Self::Error>>
        + Send
        + 'ctx
    where
        Self: 'ctx,
        Self::AcceptCtx<'ctx>: 'ctx;

    type ShutdownFuture<'ctx>: Future<Output = Result<(), Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::ShutdownCtx<'ctx>: 'ctx;

    /// 返回协议标识。
    fn scheme(&self) -> &'static str;

    /// 查询服务端通道监听地址。
    fn local_addr(&self) -> Result<TransportSocketAddr, Self::Error>;

    /// 接受新连接。
    fn accept<'ctx>(&'ctx self, ctx: &'ctx Self::AcceptCtx<'ctx>) -> Self::AcceptFuture<'ctx>;

    /// 执行优雅关闭。
    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::ShutdownCtx<'ctx>,
        direction: ShutdownDirection,
    ) -> Self::ShutdownFuture<'ctx>;
}
