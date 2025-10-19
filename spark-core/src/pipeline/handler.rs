use crate::CoreError;

use super::{context::Context, middleware::MiddlewareDescriptor};

use crate::buffer::PipelineMessage;
use crate::observability::CoreUserEvent;

/// 入站事件处理合约，面向从传输层到业务层的正向数据流。
///
/// # 设计背景（Why）
/// - 汇总 Netty `ChannelInboundHandler`、Envoy Stream Filter、gRPC Server Interceptor、Tower `Service` 调用链的经验，确保 Handler 能以细粒度响应事件。
/// - 结合科研需求，引入 `MiddlewareDescriptor` 以支持静态分析、链路可视化。
///
/// # 契约说明（What）
/// - 所有方法均在 Controller 线程或执行上下文中调用，必须无阻塞或将耗时操作移交到运行时。
/// - `on_user_event` 适配跨模块的业务事件广播。
/// - 异常需通过 `on_exception_caught` 处理，必要时触发降级或关闭连接。
///
/// # 前置/后置条件（Contract）
/// - **前置**：实现类型必须是 `'static + Send + Sync`，以便在多线程环境下安全复用。
/// - **后置**：若 `on_exception_caught` 未能恢复，应通知 Controller 触发关闭，以防资源泄漏。
///
/// # 风险提示（Trade-offs）
/// - 请避免在 Handler 内部持久化 `Context` 引用；若确有需要，需确保不会导致引用循环。
/// - `on_read` 可能在高频调用下成为性能瓶颈，可结合 `MiddlewareDescriptor::stage` 信息调度到合适线程池。
pub trait InboundHandler: Send + Sync + 'static {
    /// 返回 Handler 元数据，默认提供匿名描述，便于链路观测。
    fn describe(&self) -> MiddlewareDescriptor {
        MiddlewareDescriptor::anonymous("inbound-handler")
    }

    /// 通道活跃时调用。
    fn on_channel_active(&self, ctx: &dyn Context);

    /// 处理读到的消息。
    fn on_read(&self, ctx: &dyn Context, msg: PipelineMessage);

    /// 一批读取完成。
    fn on_read_complete(&self, ctx: &dyn Context);

    /// 可写性变化。
    fn on_writability_changed(&self, ctx: &dyn Context, is_writable: bool);

    /// 用户事件。
    fn on_user_event(&self, ctx: &dyn Context, event: CoreUserEvent);

    /// 异常处理。
    fn on_exception_caught(&self, ctx: &dyn Context, error: CoreError);

    /// 通道不再活跃。
    fn on_channel_inactive(&self, ctx: &dyn Context);
}

/// 出站事件处理合约，负责从业务层到传输层的逆向数据流。
///
/// # 设计背景（Why）
/// - 兼容 Netty `ChannelOutboundHandler`、Tower Layer、gRPC Client Interceptor、Envoy Outbound Filter 的模式。
/// - 支持科研场景中对写路径进行形式化验证或性能剖析。
///
/// # 契约说明（What）
/// - `on_write` 必须遵循背压信号语义，将 [`WriteSignal`](super::channel::WriteSignal) 返回给上游。
/// - `on_flush` 用于冲刷缓冲或触发批处理。
/// - `on_close_graceful` 需协调下游 Handler 保证截止时间内完成资源回收。
///
/// # 风险提示（Trade-offs）
/// - 若实现内部持有缓冲池，应考虑在异常路径释放资源，以免造成泄漏。
/// - 对性能敏感场景，应避免在写路径执行复杂序列化，可结合编解码器模块预处理。
pub trait OutboundHandler: Send + Sync + 'static {
    /// 返回 Handler 元数据，默认提供匿名描述。
    fn describe(&self) -> MiddlewareDescriptor {
        MiddlewareDescriptor::anonymous("outbound-handler")
    }

    /// 写入消息。
    fn on_write(
        &self,
        ctx: &dyn Context,
        msg: PipelineMessage,
    ) -> Result<super::channel::WriteSignal, CoreError>;

    /// 刷新写缓冲。
    fn on_flush(&self, ctx: &dyn Context) -> Result<(), CoreError>;

    /// 优雅关闭。
    fn on_close_graceful(
        &self,
        ctx: &dyn Context,
        deadline: Option<core::time::Duration>,
    ) -> Result<(), CoreError>;
}

/// 同时处理入站与出站事件的全双工 Handler。
///
/// # 设计背景（Why）
/// - 对齐 gRPC Bidi Stream、QUIC Stream Handler 等双向协议需求，使实现者可在单类型中处理双向逻辑。
///
/// # 契约说明（What）
/// - 任何实现 `InboundHandler + OutboundHandler` 的类型自动实现 `DuplexHandler`。
/// - 适合用于需要共享状态的编解码器、加密器或应用层协议适配器。
pub trait DuplexHandler: InboundHandler + OutboundHandler {}

impl<T> DuplexHandler for T where T: InboundHandler + OutboundHandler {}
