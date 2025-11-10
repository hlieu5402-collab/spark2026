use alloc::sync::Arc;
use core::{fmt, future::Future};

use super::{
    TransportSocketAddr, channel::Channel, handshake::HandshakeOutcome, server::ListenerShutdown,
};
use crate::{Result, pipeline::PipelineInitializer};

/// `PipelineInitializer` 选择器的统一类型别名。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 将“协议协商结果 → PipelineInitializer” 的决策过程抽象为稳定的函数接口，
///   便于不同 ServerChannel 在握手完成后复用一致的策略；
/// - 对标 Netty `ChannelInitializer` 动态选择链路的做法，使多协议监听器能够
///   根据 ALPN/SNI 等信息注入定制的中间件组合。
///
/// ## 契约（What）
/// - 输入：[`HandshakeOutcome`]，封装版本、能力与握手元数据；
/// - 输出：`Arc<dyn PipelineInitializer>`，供 ServerChannel 在连接建立后装配 Pipeline；
/// - 必须满足 `Send + Sync`，以便在多线程监听场景下安全共享。
///
/// ## 风险与注意事项（Trade-offs）
/// - 选择器返回的 `PipelineInitializer` 应为幂等配置，避免在重复调用时产生
///   竞态或资源泄漏；
/// - 若选择器内部执行阻塞操作，应在实现中自行调度至后台线程。
pub type PipelineInitializerSelector =
    dyn Fn(&HandshakeOutcome) -> Arc<dyn PipelineInitializer> + Send + Sync;

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
/// - `shutdown()`：依据 [`ListenerShutdown`] 计划执行优雅关闭；
/// - **前置条件**：上下文生命周期需覆盖 Future 执行时间；
/// - **后置条件**：Future 成功完成即表示操作符合协议语义。
///
/// ## 解析逻辑（How）
/// - 采用 GAT 约束，允许实现直接返回 `async` 块；
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

    /// 注入协议协商后如何选择 [`PipelineInitializer`] 的决策函数。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 将“协议握手结果 → PipelineInitializer” 的 L1 策略从路由层回收至监听器，
    ///   使 `ServerChannel` 在完成 ALPN/SNI 等协商后即可决定如何装配 Pipeline；
    /// - 对齐 Netty `ChannelInitializer` 的扩展点，便于多协议监听器在一个入口
    ///   根据协议族动态构建 Handler 链。
    ///
    /// ## 契约（What）
    /// - `selector`：`Arc` 包裹的决策闭包，输入为 [`HandshakeOutcome`]，输出为
    ///   `Arc<dyn PipelineInitializer>`；实现必须确保闭包本身为无状态或正确同步；
    /// - **前置条件**：调用方应在启动接受循环前设置该闭包；重复设置时以最后
    ///   一次调用为准；
    /// - **后置条件**：监听器在握手完成后应调用该闭包以取得匹配的初始化器，
    ///   并据此装配连接对应的 Pipeline。
    ///
    /// ## 解析逻辑（How）
    /// - 默认实现交由具体传输实现保存闭包引用；
    /// - `ServerChannel` 在 `accept` 成功后应读取握手元数据构造
    ///   [`HandshakeOutcome`]，再调用选择器获得初始化器。
    ///
    /// ## 风险提示（Trade-offs）
    /// - 若选择器未设置，监听器无法完成链路装配，应返回结构化错误或记录日志；
    /// - 闭包内部禁止阻塞，以免拖慢握手路径；必要时请委托后台执行器。
    fn set_initializer_selector(&self, selector: Arc<PipelineInitializerSelector>);

    /// 执行优雅关闭。
    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::ShutdownCtx<'ctx>,
        plan: ListenerShutdown,
    ) -> Self::ShutdownFuture<'ctx>;
}
