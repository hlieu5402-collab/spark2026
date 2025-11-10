use alloc::{boxed::Box, sync::Arc};

use crate::{
    CoreError, async_trait, cluster::ServiceDiscovery, context::Context, contract::CallContext,
    pipeline::Channel, sealed::Sealed,
};

use crate::pipeline::factory::{DynPipelineFactory, DynPipelineFactoryAdapter};

use super::super::{
    TransportSocketAddr, factory::ListenerConfig, intent::ConnectionIntent,
    server::ListenerShutdown, server_channel::ServerChannel,
};
use super::generic::TransportFactory as GenericTransportFactory;

/// 对象层监听器接口，供插件系统与脚本运行时存放在 `dyn` 容器中。
///
/// # 设计动机（Why）
/// - 在运行时动态注册的传输实现需要统一的对象安全接口。
/// - 与泛型层 [`ServerChannel`] 保持语义一致，便于同一实现跨层复用。
///
/// # 行为逻辑（How）
/// - `scheme_dyn` 返回协议名；
/// - `local_addr_dyn` 查询监听地址；
/// - `accept_dyn` 接受新连接并返回对象层通道；
/// - `shutdown_dyn` 执行优雅关闭并返回 `async fn` 结果；宏 [`crate::async_trait`] 会在对象层完成 Future 装箱；
/// - 适配器 [`ServerChannelObject`] 将泛型实现桥接至对象层。
///
/// # 契约说明（What）
/// - **前置条件**：监听器必须处于运行状态；
/// - **后置条件**：Future 完成即表示监听器停止接受新连接并按计划释放资源；
/// - **错误处理**：失败时返回 [`CoreError`]，调用方应记录并触发补救流程。
///
/// # 风险提示（Trade-offs）
/// - 对象层引入一次堆分配与虚表跳转；在性能敏感场景应优先考虑泛型接口。
#[async_trait]
pub trait DynServerChannel: Send + Sync + Sealed {
    /// 返回协议标识。
    fn scheme_dyn(&self) -> &'static str;

    /// 返回监听绑定地址。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 保留 [`Context`] 形参以维持接口对称，便于未来在对象层注入审计/观测逻辑；
    ///   尽管当前泛型层的 `local_addr` 已不再消费上下文，但我们仍保留该入口以避免
    ///   上层 API 发生二次破坏性变更。
    ///
    /// ## 逻辑（How）
    /// - 调用内部泛型实现的 `local_addr` 并返回其结果；
    /// - `ctx` 暂未被使用，但会在对象层保留，保持未来扩展空间。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`Context`]，当前仅用于接口统一；不会被消费。
    /// - 返回：监听器绑定的 [`TransportSocketAddr`]，失败时给出 [`CoreError`]。
    ///
    /// ## 考量（Trade-offs）
    /// - 统一签名可在动态派发中避免特判，后续若需根据 `ctx` 记录审计信息亦可拓展。
    fn local_addr_dyn(&self, ctx: &Context<'_>) -> crate::Result<TransportSocketAddr, CoreError>;

    /// 接受新的入站连接。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 在对象层保留与泛型实现一致的接受语义，使宿主可以直接获取对象安全的 [`Channel`] 实例；
    /// - 允许调用方在 `CallContext` 上施加取消/截止约束，从而控制监听器的阻塞行为。
    ///
    /// ## 逻辑（How）
    /// - 调用泛型层 [`ServerChannel::accept`]，
    ///   再将返回的通道装箱为 `Box<dyn Channel>`；
    /// - 对象层无需关心具体通道类型，仍能通过 [`Channel`] 契约执行读写或关闭操作。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`CallContext`]，提供取消与截止时间；
    /// - 返回：监听器接受到的通道及其对端地址，失败时给出结构化 [`CoreError`]。
    async fn accept_dyn(
        &self,
        ctx: &CallContext,
    ) -> crate::Result<(Box<dyn Channel>, TransportSocketAddr), CoreError>;

    /// 根据计划执行优雅关闭。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 继承调用方的执行约束，确保对象层 `async` 调用与泛型实现遵循相同的取消/超时语义。
    ///
    /// ## 逻辑（How）
    /// - 将 `ctx` 连同关闭计划透传给泛型实现，由后者处理取消/预算。
    ///
    /// ## 契约（What）
    /// - `ctx`: 执行上下文。
    /// - `plan`: 关闭计划。
    /// - 返回：遵循上下文约束的异步结果。
    ///
    /// ## 考量（Trade-offs）
    /// - 对象层无需重复解析上下文，减少重复逻辑。
    async fn shutdown_dyn(
        &self,
        ctx: &Context<'_>,
        plan: ListenerShutdown,
    ) -> crate::Result<(), CoreError>;
}

/// 将泛型监听器适配为对象层实现。
pub struct ServerChannelObject<T>
where
    T: for<'ctx> ServerChannel<
            Error = CoreError,
            AcceptCtx<'ctx> = CallContext,
            ShutdownCtx<'ctx> = Context<'ctx>,
        >,
    T::Connection: Channel + 'static,
{
    inner: Arc<T>,
}

impl<T> ServerChannelObject<T>
where
    T: for<'ctx> ServerChannel<
            Error = CoreError,
            AcceptCtx<'ctx> = CallContext,
            ShutdownCtx<'ctx> = Context<'ctx>,
        >,
    T::Connection: Channel + 'static,
{
    /// 构造新的对象层监听器包装器。
    pub fn new(inner: T) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// 取回内部泛型实现。
    pub fn into_inner(self) -> Arc<T> {
        self.inner
    }
}

#[async_trait]
impl<T> DynServerChannel for ServerChannelObject<T>
where
    T: for<'ctx> ServerChannel<
            Error = CoreError,
            AcceptCtx<'ctx> = CallContext,
            ShutdownCtx<'ctx> = Context<'ctx>,
        >,
    T::Connection: Channel + 'static,
{
    fn scheme_dyn(&self) -> &'static str {
        self.inner.scheme()
    }

    fn local_addr_dyn(&self, _ctx: &Context<'_>) -> crate::Result<TransportSocketAddr, CoreError> {
        self.inner.local_addr()
    }

    async fn accept_dyn(
        &self,
        ctx: &CallContext,
    ) -> crate::Result<(Box<dyn Channel>, TransportSocketAddr), CoreError> {
        let (connection, addr) = self.inner.accept(ctx).await?;
        Ok((Box::new(connection) as Box<dyn Channel>, addr))
    }

    async fn shutdown_dyn(
        &self,
        ctx: &Context<'_>,
        plan: ListenerShutdown,
    ) -> crate::Result<(), CoreError> {
        self.inner.shutdown(ctx, plan).await
    }
}

/// 对象层传输工厂，统一封装建连与监听流程。
///
/// # 设计动机（Why）
/// - 控制面、脚本插件可通过对象层注册自定义传输；
/// - 与泛型层 [`GenericTransportFactory`] 互转，满足 T05 “双层语义等价” 目标。
///
/// # 行为逻辑（How）
/// - `bind_dyn` 将对象层 Pipeline 工厂适配为泛型 [`DynPipelineFactoryAdapter`]，构建监听器并再度类型擦除；
/// - `connect_dyn` 调用泛型实现，返回 `Box<dyn Channel>`；两者均通过 `async fn` 暴露异步结果，由 [`crate::async_trait`] 负责装箱；
/// - 通过 `TransportFactoryObject` 在两层之间完成互转。
///
/// # 契约说明（What）
/// - **输入**：配置、Pipeline 工厂与服务发现保持与泛型层一致；
/// - **输出**：返回的监听器/通道与泛型层实现完全等价；
/// - **错误处理**：返回结构化 [`CoreError`]，调用方应结合错误码判定重试/降级策略。
///
/// # 风险提示（Trade-offs）
/// - 对象层多一次 `Box` 分配与虚表跳转，在热路径若可确定类型仍建议走泛型接口。
/// - `async_contract_overhead` 基准显示通过 `async_trait` 装箱的路径较泛型 Future 多约 0.9% CPU，整体仍满足 T05 延迟目标。
#[async_trait]
pub trait DynTransportFactory: Send + Sync + Sealed {
    /// 返回支持的协议标识。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 与泛型层保持一致，允许上层在对象层同样传递 [`Context`] 以备将来扩展。
    ///
    /// ## 逻辑（How）
    /// - 直接透传给泛型实现。
    ///
    /// ## 契约（What）
    /// - `ctx`: 执行上下文。
    /// - 返回：协议标识。
    ///
    /// ## 考量（Trade-offs）
    /// - 统一接口形式，避免桥接器需要区分对象/泛型两套签名。
    fn scheme_dyn(&self, ctx: &Context<'_>) -> &'static str;

    /// 绑定监听器。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 将取消/截止等约束传递至泛型层，确保对象安全路径不会丢失语义。
    ///
    /// ## 逻辑（How）
    /// - 创建适配器并调用泛型 `bind`，保持 `ctx` 一致。
    ///
    /// ## 契约（What）
    /// - `ctx`: 执行上下文。
    /// - `config`: 监听配置。
    /// - `pipeline_factory`: Pipeline 工厂。
    /// - 返回：对象层监听器。
    ///
    /// ## 考量（Trade-offs）
    /// - 通过 `ctx` 提供的取消信号可以避免插件长时间阻塞在构建流程。
    async fn bind_dyn(
        &self,
        ctx: &Context<'_>,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynPipelineFactory>,
    ) -> crate::Result<Box<dyn DynServerChannel>, CoreError>;

    /// 建立客户端通道。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 在对象层建连过程中传递取消/超时语义，与泛型层保持一致行为。
    ///
    /// ## 逻辑（How）
    /// - 透传 `ctx` 至泛型 `connect`，由后者控制握手与服务发现时的取消逻辑。
    ///
    /// ## 契约（What）
    /// - `ctx`: 执行上下文。
    /// - `intent`: 连接意图。
    /// - `discovery`: 可选服务发现。
    /// - 返回：对象层通道。
    ///
    /// ## 考量（Trade-offs）
    /// - 确保对象层不会悄然忽略调用方设置的截止时间。
    async fn connect_dyn(
        &self,
        ctx: &Context<'_>,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> crate::Result<Box<dyn Channel>, CoreError>;
}

/// 将泛型传输工厂适配为对象层实现。
pub struct TransportFactoryObject<F>
where
    F: GenericTransportFactory,
{
    inner: Arc<F>,
}

impl<F> TransportFactoryObject<F>
where
    F: GenericTransportFactory,
{
    /// 使用泛型实现构造对象层适配器。
    pub fn new(inner: F) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// 访问内部泛型实现的共享引用计数包装。
    pub fn into_inner(self) -> Arc<F> {
        self.inner
    }
}

#[async_trait]
impl<F> DynTransportFactory for TransportFactoryObject<F>
where
    F: GenericTransportFactory,
{
    fn scheme_dyn(&self, ctx: &Context<'_>) -> &'static str {
        self.inner.scheme(ctx)
    }

    async fn bind_dyn(
        &self,
        ctx: &Context<'_>,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynPipelineFactory>,
    ) -> crate::Result<Box<dyn DynServerChannel>, CoreError> {
        let adapter = DynPipelineFactoryAdapter::new(pipeline_factory);
        let server =
            GenericTransportFactory::bind(&*self.inner, ctx, config, Arc::new(adapter)).await?;
        Ok(Box::new(ServerChannelObject::new(server)) as Box<dyn DynServerChannel>)
    }

    async fn connect_dyn(
        &self,
        ctx: &Context<'_>,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> crate::Result<Box<dyn Channel>, CoreError> {
        let channel =
            GenericTransportFactory::connect(&*self.inner, ctx, intent, discovery).await?;
        Ok(Box::new(channel) as Box<dyn Channel>)
    }
}
