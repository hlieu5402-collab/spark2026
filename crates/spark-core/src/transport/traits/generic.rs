use alloc::sync::Arc;
use core::future::Future;

use crate::{
    CoreError, cluster::ServiceDiscovery, context::Context, pipeline::Channel, sealed::Sealed,
};

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
    type ShutdownFuture<'a>: Future<Output = crate::Result<(), CoreError>> + Send + 'a
    where
        Self: 'a;

    /// 返回本地监听地址。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 读取监听器绑定的套接字地址，供监控与注册中心上报使用。
    /// - 虽然该查询本身与取消/预算无关，但保持签名一致使得上层可以统一传递 [`Context`]。
    ///
    /// ## 逻辑（How）
    /// - 仅返回内部缓存的地址信息，不触发网络操作。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`Context`]，读取路径中的取消/超时约束；此处仅用于保持接口一致性，函数自身不会消费它。
    /// - **前置条件**：监听器已成功绑定并持有有效地址。
    /// - **后置条件**：返回的 [`TransportSocketAddr`] 恒等于实际监听地址；不修改 `ctx`。
    ///
    /// ## 考量（Trade-offs）
    /// - 统一签名避免对象层/泛型层在调度时分支判断。
    /// - 若未来需要依据 `ctx` 调整行为（例如按调用方预算选择不同指标采样），可直接复用该参数。
    fn local_addr(&self, ctx: &Context<'_>) -> TransportSocketAddr;

    /// 根据计划执行优雅关闭。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 在优雅关闭流程中继承调用方的取消、截止时间与预算约束，确保传输层遵守统一的控制语义。
    ///
    /// ## 逻辑（How）
    /// - 实现者应在 Future 内部监听 `ctx` 的取消信号或剩余预算，必要时提前终止关闭流程并返回错误。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`Context`] 视图，携带调用方的取消、截止与预算；生命周期需覆盖整个 Future。
    /// - `plan`: [`ListenerShutdown`]，定义关闭策略。
    /// - 返回：满足执行约束的 Future，完成即表示关闭完成或失败。
    ///
    /// ## 考量（Trade-offs）
    /// - 若 `ctx` 中的截止时间过短，Future 可尽早失败，以避免阻塞更高层的恢复逻辑。
    fn shutdown(&self, ctx: &Context<'_>, plan: ListenerShutdown) -> Self::ShutdownFuture<'_>;
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
    type BindFuture<'a, P>: Future<Output = crate::Result<Self::Server, CoreError>> + Send + 'a
    where
        Self: 'a,
        P: GenericControllerFactory + Send + Sync + 'static;

    /// 建连流程返回的 Future 类型。
    type ConnectFuture<'a>: Future<Output = crate::Result<Self::Channel, CoreError>> + Send + 'a
    where
        Self: 'a;

    /// 返回支持的协议标识。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 提供统一入口读取传输协议字符串，同时携带 [`Context`] 以便未来根据调用预算动态裁剪协议集合。
    ///
    /// ## 逻辑（How）
    /// - 当前实现通常直接返回静态字符串；`ctx` 仅作为占位，保持接口一致。
    ///
    /// ## 契约（What）
    /// - `ctx`: 执行上下文，当前未被消费。
    /// - 返回：协议标识的 `'static` 字符串切片。
    ///
    /// ## 考量（Trade-offs）
    /// - 统一签名避免上层针对 `scheme` 特判；未来若需按预算区分主/备协议可直接利用 `ctx`。
    fn scheme(&self, ctx: &Context<'_>) -> &'static str;

    /// 根据配置绑定监听器。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 将 [`Context`] 上的取消/截止约束下沉至监听器创建过程，避免长时间阻塞资源初始化。
    ///
    /// ## 逻辑（How）
    /// - 在 Future 内部应周期性检查 `ctx` 的取消标记或剩余预算，并据此中断或超时。
    /// - 其余逻辑与原实现一致，仅多传递 `ctx`。
    ///
    /// ## 契约（What）
    /// - `ctx`: 贯穿绑定流程的执行上下文。
    /// - `config`: [`ListenerConfig`]，描述监听参数。
    /// - `pipeline_factory`: Pipeline 装配工厂。
    /// - 返回：构建完成的 [`ServerTransport`]。
    ///
    /// ## 考量（Trade-offs）
    /// - 若绑定耗时，及时响应 `ctx` 可提升系统在缩容/回滚时的敏捷性。
    fn bind<P>(
        &self,
        ctx: &Context<'_>,
        config: ListenerConfig,
        pipeline_factory: Arc<P>,
    ) -> Self::BindFuture<'_, P>
    where
        P: GenericControllerFactory + Send + Sync + 'static,
        P::Controller: crate::pipeline::Controller;

    /// 建立客户端通道。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 将调用方的取消/截止/预算贯穿至建连过程，确保连接尝试在资源受限或超时时及时退出。
    ///
    /// ## 逻辑（How）
    /// - 实现应在等待 DNS、握手等步骤时关注 `ctx` 的状态，必要时取消操作。
    ///
    /// ## 契约（What）
    /// - `ctx`: [`Context`]，提供流程约束。
    /// - `intent`: [`ConnectionIntent`]，描述连接目标。
    /// - `discovery`: 可选服务发现依赖。
    /// - 返回：满足约束的 [`Channel`]。
    ///
    /// ## 考量（Trade-offs）
    /// - 融合 `ctx` 可避免重复传参（取消标记、超时时间）并与 Service 层保持一致。
    fn connect(
        &self,
        ctx: &Context<'_>,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> Self::ConnectFuture<'_>;
}
