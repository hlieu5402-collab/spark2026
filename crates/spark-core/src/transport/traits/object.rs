use alloc::{boxed::Box, sync::Arc};

use crate::{
    CoreError, async_trait, cluster::ServiceDiscovery, context::ExecutionContext,
    pipeline::Channel, sealed::Sealed,
};

use crate::pipeline::traits::object::{DynControllerFactory, DynControllerFactoryAdapter};

use super::super::{
    TransportSocketAddr, factory::ListenerConfig, intent::ConnectionIntent,
    server::ListenerShutdown,
};
use super::generic::{
    ServerTransport as GenericServerTransport, TransportFactory as GenericTransportFactory,
};

/// 对象层监听器接口，供插件系统与脚本运行时存放在 `dyn` 容器中。
///
/// # 设计动机（Why）
/// - 在运行时动态注册的传输实现需要统一的对象安全接口。
/// - 与泛型层 [`GenericServerTransport`] 保持语义一致，便于同一实现跨层复用。
///
/// # 行为逻辑（How）
/// - `local_addr_dyn` 返回监听地址；
/// - `shutdown_dyn` 执行优雅关闭并返回 `async fn` 结果；宏 [`crate::async_trait`] 会在对象层完成 Future 装箱；
/// - 适配器 [`ServerTransportObject`] 将泛型实现桥接至对象层。
///
/// # 契约说明（What）
/// - **前置条件**：监听器必须处于运行状态；
/// - **后置条件**：Future 完成即表示监听器停止接受新连接并按计划释放资源；
/// - **错误处理**：失败时返回 [`CoreError`]，调用方应记录并触发补救流程。
///
/// # 风险提示（Trade-offs）
/// - 对象层引入一次堆分配与虚表跳转；在性能敏感场景应优先考虑泛型接口。
#[async_trait]
pub trait DynServerTransport: Send + Sync + Sealed {
    /// 返回监听绑定地址。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 维持与泛型层一致的上下文签名，使对象层桥接器可无差异透传 [`ExecutionContext`]。
    ///
    /// ## 逻辑（How）
    /// - 调用内部泛型实现的 `local_addr`，并将 `ctx` 原样传递。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`ExecutionContext`]，当前仅用于接口统一；不会被消费。
    /// - 返回：监听器绑定的 [`TransportSocketAddr`]。
    ///
    /// ## 考量（Trade-offs）
    /// - 统一签名可在动态派发中避免特判，后续若需根据 `ctx` 记录审计信息亦可拓展。
    fn local_addr_dyn(&self, ctx: &ExecutionContext<'_>) -> TransportSocketAddr;

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
        ctx: &ExecutionContext<'_>,
        plan: ListenerShutdown,
    ) -> crate::Result<(), CoreError>;
}

/// 将泛型监听器适配为对象层实现。
pub struct ServerTransportObject<T>
where
    T: GenericServerTransport,
{
    inner: Arc<T>,
}

impl<T> ServerTransportObject<T>
where
    T: GenericServerTransport,
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
impl<T> DynServerTransport for ServerTransportObject<T>
where
    T: GenericServerTransport,
{
    fn local_addr_dyn(&self, ctx: &ExecutionContext<'_>) -> TransportSocketAddr {
        self.inner.local_addr(ctx)
    }

    async fn shutdown_dyn(
        &self,
        ctx: &ExecutionContext<'_>,
        plan: ListenerShutdown,
    ) -> crate::Result<(), CoreError> {
        GenericServerTransport::shutdown(&*self.inner, ctx, plan).await
    }
}

/// 对象层传输工厂，统一封装建连与监听流程。
///
/// # 设计动机（Why）
/// - 控制面、脚本插件可通过对象层注册自定义传输；
/// - 与泛型层 [`GenericTransportFactory`] 互转，满足 T05 “双层语义等价” 目标。
///
/// # 行为逻辑（How）
/// - `bind_dyn` 将对象层 Pipeline 工厂适配为泛型 [`DynControllerFactoryAdapter`]，构建监听器并再度类型擦除；
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
    /// - 与泛型层保持一致，允许上层在对象层同样传递 [`ExecutionContext`] 以备将来扩展。
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
    fn scheme_dyn(&self, ctx: &ExecutionContext<'_>) -> &'static str;

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
        ctx: &ExecutionContext<'_>,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynControllerFactory>,
    ) -> crate::Result<Box<dyn DynServerTransport>, CoreError>;

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
        ctx: &ExecutionContext<'_>,
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
    fn scheme_dyn(&self, ctx: &ExecutionContext<'_>) -> &'static str {
        self.inner.scheme(ctx)
    }

    async fn bind_dyn(
        &self,
        ctx: &ExecutionContext<'_>,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynControllerFactory>,
    ) -> crate::Result<Box<dyn DynServerTransport>, CoreError> {
        let adapter = DynControllerFactoryAdapter::new(pipeline_factory);
        let server =
            GenericTransportFactory::bind(&*self.inner, ctx, config, Arc::new(adapter)).await?;
        Ok(Box::new(ServerTransportObject::new(server)) as Box<dyn DynServerTransport>)
    }

    async fn connect_dyn(
        &self,
        ctx: &ExecutionContext<'_>,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> crate::Result<Box<dyn Channel>, CoreError> {
        let channel =
            GenericTransportFactory::connect(&*self.inner, ctx, intent, discovery).await?;
        Ok(Box::new(channel) as Box<dyn Channel>)
    }
}
