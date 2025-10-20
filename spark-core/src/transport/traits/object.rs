use alloc::{boxed::Box, sync::Arc};

use crate::{CoreError, async_trait, cluster::ServiceDiscovery, pipeline::Channel, sealed::Sealed};

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
    fn local_addr_dyn(&self) -> TransportSocketAddr;

    /// 根据计划执行优雅关闭。
    async fn shutdown_dyn(&self, plan: ListenerShutdown) -> Result<(), CoreError>;
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
    fn local_addr_dyn(&self) -> TransportSocketAddr {
        self.inner.local_addr()
    }

    async fn shutdown_dyn(&self, plan: ListenerShutdown) -> Result<(), CoreError> {
        GenericServerTransport::shutdown(&*self.inner, plan).await
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
    fn scheme_dyn(&self) -> &'static str;

    /// 绑定监听器。
    async fn bind_dyn(
        &self,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynControllerFactory>,
    ) -> Result<Box<dyn DynServerTransport>, CoreError>;

    /// 建立客户端通道。
    async fn connect_dyn(
        &self,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> Result<Box<dyn Channel>, CoreError>;
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
    fn scheme_dyn(&self) -> &'static str {
        self.inner.scheme()
    }

    async fn bind_dyn(
        &self,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn DynControllerFactory>,
    ) -> Result<Box<dyn DynServerTransport>, CoreError> {
        let adapter = DynControllerFactoryAdapter::new(pipeline_factory);
        let server = GenericTransportFactory::bind(&*self.inner, config, Arc::new(adapter)).await?;
        Ok(Box::new(ServerTransportObject::new(server)) as Box<dyn DynServerTransport>)
    }

    async fn connect_dyn(
        &self,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> Result<Box<dyn Channel>, CoreError> {
        let channel = GenericTransportFactory::connect(&*self.inner, intent, discovery).await?;
        Ok(Box::new(channel) as Box<dyn Channel>)
    }
}
