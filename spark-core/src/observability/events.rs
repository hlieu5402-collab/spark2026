use crate::{
    BoxStream,
    cluster::{ClusterMembershipEvent, DiscoveryEvent},
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::time::Duration;

/// 核心用户事件，供 Handler 或服务层之间传递标准化信号。
///
/// # 设计背景（Why）
/// - 替代 `Box<dyn Any>` 的不透明事件传递方式，确保跨组件通信具有可推理的契约。
/// - 参考 Envoy、Linkerd 在连接事件模型上的设计，涵盖 TLS、空闲、限速等关键场景。
///
/// # 契约说明（What）
/// - **前置条件**：生产者在广播事件前需确保相应消费者已注册处理逻辑。
/// - **后置条件**：事件对象应视为不可变；若需修改请创建新事件。
///
/// # 风险提示（Trade-offs）
/// - `ApplicationSpecific` 变体提供开放扩展，但需在上层维护良好命名空间以避免冲突。
#[derive(Debug, Clone)]
pub enum CoreUserEvent {
    TlsEstablished(TlsInfo),
    IdleTimeout(IdleTimeout),
    RateLimited(RateLimited),
    ConfigChanged { keys: Vec<String> },
    ApplicationSpecific(Arc<dyn core::any::Any + Send + Sync>),
}

/// TLS 握手完成时的上下文信息。
///
/// # 契约说明（What）
/// - `version`、`cipher` 记录协商结果；`peer_identity` 可包含 SPIFFE ID 或 SAN。
/// - **风险提示**：避免记录完整证书等敏感信息，可仅保留哈希或标识符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInfo {
    pub version: String,
    pub cipher: String,
    pub peer_identity: Option<String>,
}

/// 空闲超时方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDirection {
    Read,
    Write,
    Both,
}

/// 空闲超时事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeout {
    pub direction: IdleDirection,
}

/// 限速方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDirection {
    Inbound,
    Outbound,
}

/// 限速事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    pub direction: RateDirection,
}

/// 运维事件枚举，聚合运行期关键状态变更。
///
/// # 设计背景（Why）
/// - 融合 Netflix Zuul、Envoy Hot Restart 等实践，提供背压、集群变更、健康状态更新等关键信号。
/// - 支持云原生与边缘场景，通过可选字段兼容不同部署模式。
///
/// # 契约说明（What）
/// - **前置条件**：生产者应限制事件频率，必要时做去抖或节流。
/// - **后置条件**：事件总线实现需保证有界缓冲，并在溢出时提供监控信号。
#[derive(Clone, Debug)]
pub enum OpsEvent {
    ShutdownTriggered {
        deadline: Duration,
    },
    BackpressureApplied {
        channel_id: String,
    },
    FailureClusterDetected {
        error_code: &'static str,
        count: u64,
    },
    ClusterChange(ClusterMembershipEvent),
    DiscoveryJitter(DiscoveryEvent),
    BufferLeakDetected {
        count: usize,
    },
    HealthChange(super::health::ComponentHealth),
}

/// 运维事件总线契约。
///
/// # 设计背景（Why）
/// - 借鉴 Google BorgMon 与现代事件溯源系统，要求广播非阻塞且支持多个观察者。
/// - 允许研究型实现（如基于 CRDT 的事件流）在遵循接口的前提下接入。
///
/// # 契约说明（What）
/// - **前置条件**：`broadcast` 调用需快速返回；如需阻塞应在内部异步处理。
/// - **后置条件**：`subscribe` 返回的流必须具备背压或丢弃策略，避免慢消费者拖垮系统。
///
/// # 风险提示（Trade-offs）
/// - 建议实现对订阅者生命周期做监控，避免内存泄漏。
pub trait OpsEventBus: Send + Sync + 'static {
    /// 广播事件。
    fn broadcast(&self, event: OpsEvent);

    /// 订阅事件流。
    fn subscribe(&self) -> BoxStream<'static, OpsEvent>;
}
