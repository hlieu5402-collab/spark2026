use alloc::sync::Arc;
use core::future::Future;

use crate::{CoreError, cluster::ServiceDiscovery, pipeline::Channel, sealed::Sealed};

use crate::pipeline::traits::generic::ControllerFactory as GenericControllerFactory;

use super::super::{
    TransportSocketAddr, factory::ListenerConfig, intent::ConnectionIntent,
    server::ListenerShutdown,
};

/// 泛型层的监听器合约，描述服务端优雅关闭语义。
///
/// # 设计初衷（Why）
/// - 为内建传输实现提供零虚分派路径，在热路径（大规模建连/关闭）下避免对象层额外开销。
/// - 保持与对象层 [`super::object::DynServerTransport`] 等价的语义，方便插件或脚本重用相同行为。
///
/// # 行为逻辑（How）
/// - `local_addr` 返回监听绑定地址，供注册中心或观测模块使用；
/// - `shutdown` 接收 [`ListenerShutdown`]，执行优雅关闭并返回可等待的 Future；
/// - Future 的具体类型由实现决定，调用方在泛型层可以零成本等待。
///
/// # 契约说明（What）
/// - **前置条件**：调用前监听器必须成功启动；
/// - **后置条件**：Future 成功完成表示监听器不再接受新连接，并按照计划处理既有会话；
/// - **错误处理**：失败时返回结构化 [`CoreError`]，应包含错误码与诊断信息。
///
/// # 风险提示（Trade-offs）
/// - 若底层平台不支持优雅关闭，需在 Future 中返回明确错误，避免调用方误判；
/// - 实现者应关注关闭流程中的资源释放，防止内存/句柄泄漏。
pub trait ServerTransport: Send + Sync + 'static + Sealed {
    /// 关闭 Future 的具体类型。
    type ShutdownFuture<'a>: Future<Output = Result<(), CoreError>> + Send + 'a
    where
        Self: 'a;

    /// 返回本地监听地址。
    fn local_addr(&self) -> TransportSocketAddr;

    /// 根据计划执行优雅关闭。
    fn shutdown(&self, plan: ListenerShutdown) -> Self::ShutdownFuture<'_>;
}

/// 泛型层传输工厂，统一建连与监听流程。
///
/// # 设计初衷（Why）
/// - 内建实现可完全使用泛型接口，避免 `BoxFuture` 与 trait object 带来的运行时开销；
/// - 通过与对象层适配器互转，保证插件/脚本仍可复用同一实现。
///
/// # 行为逻辑（How）
/// - `scheme` 返回协议标识（如 `tcp`/`quic`）；
/// - `bind` 结合 [`ListenerConfig`] 与 Pipeline 工厂构造监听器，返回具体 [`ServerTransport`]；
/// - `connect` 根据 [`ConnectionIntent`] 建立客户端通道，返回实现 [`Channel`] 的类型；
/// - 需要与 [`Arc<GenericControllerFactory>`] 协同，以装配 Pipeline。
///
/// # 契约说明（What）
/// - **关联类型**：`Channel`/`Server` 必须分别实现 [`Channel`] 与 [`ServerTransport`]；
/// - **前置条件**：调用方需保证 `ListenerConfig::endpoint` 与工厂 `scheme` 匹配；
/// - **后置条件**：成功返回的监听器/通道在语义上等同于对象层的 `Dyn` 版本；
/// - **错误处理**：失败需返回 [`CoreError`]，并填写错误码提示运维动作。
///
/// # 风险提示（Trade-offs）
/// - 泛型层 Future 类型通常较复杂，若需在对象层复用，请结合 [`crate::pipeline::traits::object::DynControllerFactoryAdapter`] 等适配器做类型擦除；
/// - 建连过程若依赖 `ServiceDiscovery`，应处理网络分区、陈旧快照等异常并返回语义化错误。
pub trait TransportFactory: Send + Sync + 'static + Sealed {
    /// 客户端通道类型。
    type Channel: Channel;
    /// 服务端监听器类型。
    type Server: ServerTransport;

    /// 绑定流程返回的 Future 类型。
    type BindFuture<'a, P>: Future<Output = Result<Self::Server, CoreError>> + Send + 'a
    where
        Self: 'a,
        P: GenericControllerFactory + Send + Sync + 'static;

    /// 建连流程返回的 Future 类型。
    type ConnectFuture<'a>: Future<Output = Result<Self::Channel, CoreError>> + Send + 'a
    where
        Self: 'a;

    /// 返回支持的协议标识。
    fn scheme(&self) -> &'static str;

    /// 根据配置绑定监听器。
    fn bind<P>(&self, config: ListenerConfig, pipeline_factory: Arc<P>) -> Self::BindFuture<'_, P>
    where
        P: GenericControllerFactory + Send + Sync + 'static,
        P::Controller: crate::pipeline::Controller;

    /// 建立客户端通道。
    fn connect(
        &self,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> Self::ConnectFuture<'_>;
}
