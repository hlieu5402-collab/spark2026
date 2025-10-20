use super::{Endpoint, TransportParams};
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
/// - **前置条件**：`endpoint` 应包含可解析的主机与端口；调用前应完成权限校验。
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

// 传输工厂的泛型与对象层接口已迁移至 `crate::transport::traits`：
// - 泛型接口：`crate::transport::traits::generic::TransportFactory`
// - 对象接口：`crate::transport::traits::object::DynTransportFactory`
