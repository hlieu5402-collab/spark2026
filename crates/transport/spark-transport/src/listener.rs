use core::future::Future;

use crate::{ShutdownDirection, TransportConnection, TransportSocketAddr};

/// 统一的传输监听器接口。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 在 TCP/TLS/QUIC 等监听实现之间共享统一签名，便于宿主在运行期热切换协议；
/// - 将绑定、接受、关闭流程纳入一个 trait，确保调度器与运维脚本在替换实现时无需改动。
///
/// ## 架构定位（Architecture）
/// - 监听器位于传输工厂与具体协议实现之间，负责产出 [`TransportConnection`]；
/// - 调用方可针对该 trait 编写通用逻辑，如并行接受、优雅关闭等操作。
///
/// ## 契约说明（What）
/// - `AcceptCtx<'ctx>`：接受连接时使用的上下文（如 `CallContext`）；
/// - `ShutdownCtx<'ctx>`：关闭监听器时使用的上下文（如 `ExecutionContext<'ctx>`）；
/// - `accept` 返回 `(Connection, TransportSocketAddr)`，其中地址代表对端元数据；
/// - `shutdown` 使用 [`ShutdownDirection`] 描述优雅关闭策略；
/// - `scheme` 返回协议标识字符串。
///
/// ## 风险提示（Trade-offs）
/// - 若协议不支持优雅关闭，应在 `shutdown` 中返回错误或记录限制；
/// - 监听器应确保 `accept` 在上下文取消时迅速返回，避免阻塞缩容流程。
pub trait TransportListener: Send + Sync + 'static {
    /// 错误类型。
    type Error: core::fmt::Debug + Send + Sync + 'static;
    /// 接受连接使用的上下文类型。
    type AcceptCtx<'ctx>: ?Sized;
    /// 关闭监听器使用的上下文类型。
    type ShutdownCtx<'ctx>: ?Sized;
    /// 监听器生成的连接类型。
    type Connection: TransportConnection<Error = Self::Error>;

    /// 接受连接的 Future 类型。
    type AcceptFuture<'ctx>: Future<Output = Result<(Self::Connection, TransportSocketAddr), Self::Error>>
        + Send
        + 'ctx
    where
        Self: 'ctx,
        Self::AcceptCtx<'ctx>: 'ctx;

    /// 优雅关闭的 Future 类型。
    type ShutdownFuture<'ctx>: Future<Output = Result<(), Self::Error>> + Send + 'ctx
    where
        Self: 'ctx,
        Self::ShutdownCtx<'ctx>: 'ctx;

    /// 返回协议标识（例如 `"tcp"`、`"quic"`）。
    fn scheme(&self) -> &'static str;

    /// 查询监听器实际绑定的地址。
    fn local_addr(&self) -> Result<TransportSocketAddr, Self::Error>;

    /// 接受一个入站连接。
    fn accept<'ctx>(&'ctx self, ctx: &'ctx Self::AcceptCtx<'ctx>) -> Self::AcceptFuture<'ctx>;

    /// 根据上下文执行优雅关闭。
    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::ShutdownCtx<'ctx>,
        direction: ShutdownDirection,
    ) -> Self::ShutdownFuture<'ctx>;
}
