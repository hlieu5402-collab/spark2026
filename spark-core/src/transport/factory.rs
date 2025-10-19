use alloc::{boxed::Box, sync::Arc};

use crate::{
    BoxFuture, SparkError,
    cluster::ServiceDiscovery,
    pipeline::{Channel, ControllerFactory},
};

use super::{ConnectionIntent, Endpoint, ServerTransport, TransportParams};
use core::time::Duration;

/// 监听配置，描述传输工厂如何在目标端点上暴露服务。
///
/// # 设计背景（Why）
/// - 综合 Envoy Listener、Netty ServerBootstrap、Tokio Listener Options，将常见配置项（并发、积压、参数）标准化。
/// - 支撑科研实验：`params` 可注入拥塞控制、负载调度策略标识，方便对比试验。
///
/// # 契约说明（What）
/// - `endpoint`：监听的物理端点，必须是 [`EndpointKind::Physical`](super::EndpointKind::Physical)。
/// - `params`：额外的键值参数（如 `tcp_backlog`、`quic_stateless_retry`）。
/// - `concurrency_limit`：建议的最大并发连接数，`None` 表示交由实现决定。
/// - `accept_backoff`：接受新连接的退避策略（如限流或防御攻击）。
/// - **前置条件**：`endpoint` 应该包含可解析的主机与端口；调用前应完成权限校验。
/// - **后置条件**：当 `bind` 返回成功时，监听器应按照配置开始接收连接。
///
/// # 风险提示（Trade-offs）
/// - 配置未对参数合法性做严格校验，需由上层或实现方验证。
/// - 并发限制过低可能导致连接饥饿；过高则可能耗尽资源。
#[derive(Clone, Debug)]
pub struct ListenerConfig {
    endpoint: Endpoint,
    params: TransportParams,
    concurrency_limit: Option<u32>,
    accept_backoff: Option<Duration>,
}

impl ListenerConfig {
    /// 以必需的端点构造配置。
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            params: TransportParams::new(),
            concurrency_limit: None,
            accept_backoff: None,
        }
    }

    /// 指定最大并发连接数。
    pub fn with_concurrency_limit(mut self, limit: u32) -> Self {
        self.concurrency_limit = Some(limit);
        self
    }

    /// 指定接受退避时间。
    pub fn with_accept_backoff(mut self, backoff: Duration) -> Self {
        self.accept_backoff = Some(backoff);
        self
    }

    /// 覆盖参数表。
    pub fn with_params(mut self, params: TransportParams) -> Self {
        self.params = params;
        self
    }

    /// 访问端点。
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 访问参数。
    pub fn params(&self) -> &TransportParams {
        &self.params
    }

    /// 获取并发上限。
    pub fn concurrency_limit(&self) -> Option<u32> {
        self.concurrency_limit
    }

    /// 获取退避时间。
    pub fn accept_backoff(&self) -> Option<Duration> {
        self.accept_backoff
    }
}

/// 传输工厂统一封装绑定与连接流程。
///
/// # 设计背景（Why）
/// - **生产经验**：与 Netty `ChannelFactory`、Tower `MakeService` 一致，在运行时选择不同协议实现（TCP、QUIC、内存）。
/// - **科研探索**：结合服务发现（ServiceDiscovery）实现 Intent-Based 连接策略，允许替换为仿真或调度算法原型。
///
/// # 契约说明（What）
/// - `scheme`：工厂支持的协议方案，如 `tcp`、`quic`、`ws`。
/// - `bind`：根据 [`ListenerConfig`] 与管线工厂创建监听器。
/// - `connect`：基于 [`ConnectionIntent`] 构建客户端通道，可选结合服务发现。
/// - **前置条件**：调用方需确保 `endpoint.scheme()` 与工厂匹配，否则返回 `SparkError::unsupported_protocol` 等语义化错误。
/// - **后置条件**：成功时返回动态分发的监听器或通道，生命周期由调用方管理。
///
/// # 风险提示（Trade-offs）
/// - 建连可能涉及 DNS、服务发现、握手，多步异步流程需尊重 `timeout` 与 `retry_budget`。
/// - 绑定失败需提供明确错误原因，便于运维排查（端口占用、权限不足等）。
pub trait TransportFactory: Send + Sync + 'static {
    /// 返回支持的 scheme。
    fn scheme(&self) -> &'static str;

    /// 绑定服务端端点。
    fn bind(
        &self,
        config: ListenerConfig,
        pipeline_factory: Arc<dyn ControllerFactory>,
    ) -> BoxFuture<'static, Result<Box<dyn ServerTransport>, SparkError>>;

    /// 连接客户端端点。
    fn connect(
        &self,
        intent: ConnectionIntent,
        discovery: Option<Arc<dyn ServiceDiscovery>>,
    ) -> BoxFuture<'static, Result<Box<dyn Channel>, SparkError>>;
}
