use alloc::sync::Arc;
use core::future::Future;

use crate::{
    CoreError, cluster::ServiceDiscovery, context::Context, contract::CallContext,
    pipeline::Channel, sealed::Sealed,
};

use crate::pipeline::factory::PipelineFactory as GenericPipelineFactory;

use super::super::{
    factory::ListenerConfig, intent::ConnectionIntent, server_channel::ServerChannel,
};

/// 泛型层传输工厂，统一建连与监听流程。
///
/// # 契约维度速览
/// - **语义**：向上暴露“协议标识 + 监听器 + 客户端通道”三元组，负责在 Transport 与 Pipeline 之间桥接控制面。
/// - **错误**：所有失败需返回 [`CoreError`]，推荐错误码：`transport.scheme_mismatch`、`transport.connect_failed`、`transport.bind_failed`。
/// - **并发**：工厂实现必须 `Send + Sync`；`bind`/`connect` 可在多个异步任务中同时调用，内部需确保资源竞态被序列化。
/// - **背压**：当内部发现资源紧张（线程池、FD 用尽），应返回 `CoreError` 并建议上层传播 [`BackpressureSignal::Busy`](crate::contract::BackpressureSignal::Busy)。
/// - **超时**：所有方法通过 [`Context`] 接收截止时间；实现应在 Future 内定期检查并在超时后返回 `CoreError`。
/// - **取消**：[`Context`] 携带取消标记；`bind`/`connect` 的 Future 需在取消时尽快退出并回滚部分初始化。
/// - **观测标签**：建议上报 `transport.scheme`、`transport.intent`、`transport.listener_addr`、`transport.channel_peer` 等标签。
/// - **示例(伪码)**：
///   ```text
///   factory = registry.pick("tcp")
///   server = await factory.bind(ctx, config, pipeline_factory)
///   channel = await factory.connect(ctx, intent, discovery)
///   ```
///
/// # 设计初衷（Why）
/// - 内建实现可完全使用泛型接口，避免 `BoxFuture` 与 trait object 带来的运行时开销；
/// - 通过与对象层适配器互转，保证插件/脚本仍可复用同一实现。
///
/// # 行为逻辑（How）
/// - `scheme` 返回协议标识（如 `tcp`/`quic`）；
/// - `bind` 结合 [`ListenerConfig`] 与 Pipeline 工厂构造监听器，返回具体 [`ServerChannel`]；
/// - `connect` 根据 [`ConnectionIntent`] 建立客户端通道，返回实现 [`Channel`] 的类型；
/// - 需要与 [`Arc<GenericPipelineFactory>`] 协同，以装配 Pipeline。
///
/// # 契约说明（What）
/// - **关联类型**：`Channel`/`Server` 必须分别实现 [`Channel`] 与 [`ServerChannel`]；
/// - **前置条件**：调用方需保证 `ListenerConfig::endpoint` 与工厂 `scheme` 匹配；
/// - **后置条件**：成功返回的监听器/通道在语义上等同于对象层的 `Dyn` 版本；
/// - **错误处理**：失败需返回 [`CoreError`]，并填写错误码提示运维动作。
///
/// # 风险提示（Trade-offs）
/// - 泛型层 Future 类型通常较复杂，若需在对象层复用，请结合 [`crate::pipeline::factory::DynPipelineFactoryAdapter`] 等适配器做类型擦除；
/// - 建连过程若依赖 `ServiceDiscovery`，应处理网络分区、陈旧快照等异常并返回语义化错误。
pub trait TransportFactory: Send + Sync + 'static + Sealed {
    /// 客户端通道类型。
    type Channel: Channel;
    /// 服务端监听器类型。
    type Server: for<'ctx> ServerChannel<
            Error = CoreError,
            Connection = Self::Channel,
            AcceptCtx<'ctx> = CallContext,
            ShutdownCtx<'ctx> = Context<'ctx>,
        >;

    /// 绑定流程返回的 Future 类型。
    type BindFuture<'a, P>: Future<Output = crate::Result<Self::Server, CoreError>> + Send + 'a
    where
        Self: 'a,
        P: GenericPipelineFactory + Send + Sync + 'static;

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
    /// - 返回：构建完成的 [`ServerChannel`]，其中 `Connection` 等于 `Self::Channel`，确保监听器接收的通道类型与客户端建连保持一致。
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
        P: GenericPipelineFactory + Send + Sync + 'static,
        P::Pipeline: crate::pipeline::Pipeline;

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
