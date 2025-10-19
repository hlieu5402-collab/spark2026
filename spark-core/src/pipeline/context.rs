use super::{channel::Channel, controller::Controller};
use crate::{
    buffer::{BufferPool, PipelineMessage},
    cluster::{ClusterMembership, ServiceDiscovery},
    context::ExecutionContext,
    contract::{CallContext, CloseReason, Deadline},
    observability::{Logger, MetricsProvider, TraceContext},
    runtime::{TaskExecutor, TimeDriver},
};

/// Handler 访问运行时能力与事件流的统一入口。
///
/// # 设计背景（Why）
/// - 融合 Netty `ChannelHandlerContext`、Tower `ServiceContext`、Envoy Filter Callback、Akka `ActorContext` 的设计理念，提供集中化入口减少耦合。
/// - 通过对象安全 Trait，支持动态装配 Handler/Middleware，同时保留 `no_std` 可用性。
///
/// # 契约说明（What）
/// - `channel` / `controller`：返回当前连接与控制面引用，便于查询状态或转发事件。
/// - `executor` / `timer`：异步调度能力，保障 Handler 中长耗时操作不会阻塞事件循环。
/// - `buffer_pool`：租借编解码缓冲，需遵循“租借即还”原则。
/// - `trace_context` / `metrics` / `logger`：可观测性三件套，方便 Handler 打点、打日志、串联分布式追踪。
/// - `membership` / `discovery`：分布式能力入口，允许 Handler 做路由或副本选择。
/// - `forward_read`：继续向后传递读事件，遵循责任链模式。
/// - `write` / `flush` / `close_graceful`：与 [`Channel`] 一致的写与关闭语义。
///
/// # 前置/后置条件（Contract）
/// - **前置**：调用者应在事件回调内部使用 Context；跨线程持有引用需要实现保证线程安全。
/// - **后置**：`write` 返回 [`crate::pipeline::WriteSignal`]，调用方需根据反馈调整速率；`close_graceful` 必须确保控制器收到关闭事件。
///
/// # 风险提示（Trade-offs）
/// - 若实现使用 `Rc`/`RefCell` 等单线程结构，将无法满足 `Send + Sync` 要求，应在构造阶段检测。
/// - `forward_read` 在 Handler 链中是立即调用的同步行为，重计算或阻塞逻辑应移交给 `executor()`。
pub trait Context: Send + Sync {
    /// 当前通道引用。
    fn channel(&self) -> &dyn Channel;

    /// 当前控制器引用。
    fn controller(&self) -> &dyn Controller;

    /// 执行器引用。
    fn executor(&self) -> &dyn TaskExecutor;

    /// 计时器引用。
    fn timer(&self) -> &dyn TimeDriver;

    /// 缓冲池访问接口。
    ///
    /// # 契约说明
    /// - 返回值必须实现 [`BufferPool`]，供 Handler 在编解码过程中租借/归还缓冲。
    /// - 调用方不得缓存引用超过事件回调生命周期，避免破坏池的自适应调度。
    fn buffer_pool(&self) -> &dyn BufferPool;

    /// 链路追踪上下文。
    fn trace_context(&self) -> &TraceContext;

    /// 指标提供者。
    fn metrics(&self) -> &dyn MetricsProvider;

    /// 日志器。
    fn logger(&self) -> &dyn Logger;

    /// 集群成员能力。
    fn membership(&self) -> Option<&dyn ClusterMembership>;

    /// 服务发现能力。
    fn discovery(&self) -> Option<&dyn ServiceDiscovery>;

    /// 当前调用上下文，携带取消/截止/预算等统一契约。
    fn call_context(&self) -> &CallContext;

    /// 快速派生三元组视图，减少热路径中的上下文克隆。
    ///
    /// # 设计背景（Why）
    /// - Handler 在执行 `poll_ready`、预算检查等操作时，只需访问取消/截止/预算三元组，
    ///   若强制克隆整份 [`CallContext`] 将带来多余的 `Arc` 管理成本。
    ///
    /// # 契约说明（What）
    /// - **前置条件**：`self` 必须保证返回的 [`CallContext`] 在 `ExecutionContext` 生命周期内有效；
    /// - **后置条件**：返回值为 [`ExecutionContext`] 只读视图，禁止通过该视图修改预算或取消状态。
    fn execution_context(&self) -> ExecutionContext<'_> {
        self.call_context().execution()
    }

    /// 继续向后传播读事件。
    fn forward_read(&self, msg: PipelineMessage);

    /// 写消息。
    fn write(&self, msg: PipelineMessage) -> Result<super::WriteSignal, crate::CoreError>;

    /// 刷新缓冲。
    fn flush(&self);

    /// 优雅关闭。
    fn close_graceful(&self, reason: CloseReason, deadline: Option<Deadline>);

    /// 等待关闭完成。
    fn closed(&self) -> crate::future::BoxFuture<'static, Result<(), crate::SparkError>>;
}
