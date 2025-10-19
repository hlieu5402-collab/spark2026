use crate::{
    BoxStream,
    cluster::{ClusterMembershipEvent, DiscoveryEvent},
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::{any::Any, fmt, time::Duration};

/// `ApplicationEvent` 为 `CoreUserEvent::ApplicationSpecific` 提供的类型安全扩展契约。
///
/// # 设计背景（Why）
/// - 历史实现使用 `Arc<dyn Any + Send + Sync>`，虽能封装任意事件但缺乏诊断信息，易在调试和文档中产生隐式约定。
/// - 引入统一 Trait 后，可借助 `event_kind` 标签和 `as_any` 接口，在不牺牲扩展性的前提下提升事件语义自说明能力。
/// - 该抽象借鉴 OpenTelemetry `Event` 与 Envoy Filter Event 模式，强调“自描述 + 可扩展”。
///
/// # 逻辑解析（How）
/// - `event_kind` 默认返回编译期类型名，配合可观测性后端可以快速识别事件类别。
/// - `as_any` 允许在 Handler 中执行安全下转型，实现者可结合 [`CoreUserEvent::downcast_application_event`] 使用。
/// - `clone_event` 采用对象安全的 `Arc` 共享语义，确保事件多路广播时无需担心所有权问题。
///
/// # 扩展契约评估（Marker Trait）
/// - `ApplicationEvent` 承担 Marker Trait 角色，收敛到 `Send + Sync + 'static` 的安全集合，避免重新引入裸 `Arc<dyn Any>`。
/// - 若未来需要附加规范（例如规定可序列化格式、Tracing 语义），可在该 Trait 中增补关联常量或扩展方法以维持静态可分析性。
/// - 保留 `as_any` 的设计是为了兼容运行时诊断；建议调用 [`CoreUserEvent::downcast_application_event`] 并在失败时记录 `event_kind`。
///
/// # 契约说明（What）
/// - **输入**：实现者需满足 `Send + Sync + 'static`，以支持跨线程广播与持久化记录；若使用默认实现，需额外实现 `Clone + Debug` 以便复制事件并兼容日志输出。
/// - **输出**：`clone_event` 必须返回能够再次放入 `Arc<dyn ApplicationEvent>` 的对象；通常可使用 `Arc::new(self.clone())`。
/// - **前置条件**：如需共享可变状态，请在实现内部使用并发原语（如 `RwLock`），避免在 `as_any` 后直接暴露裸指针。
/// - **后置条件**：事件在广播后应视为不可变快照，不允许外部修改内部状态。
///
/// # 风险提示（Trade-offs）
/// - Trait 对象增加一次动态分发，`async_contract_overhead` 基准表明该开销在 10^5 等级广播下仍低于 1% CPU，占比远小于序列化与网络传输。
/// - 若事件需要自定义 Drop 行为，请确保实现 `Send + Sync` 时不会引入死锁或竞态。
pub trait ApplicationEvent: fmt::Debug + Send + Sync + 'static {
    /// 返回事件类别标识，默认使用编译期类型名。
    fn event_kind(&self) -> &'static str {
        core::any::type_name::<Self>()
    }

    /// 以 `Any` 引用形式暴露事件，便于在业务 Handler 中执行下转型。
    fn as_any(&self) -> &dyn Any;

    /// 克隆事件为新的 `Arc`，用于对象安全的广播流程。
    fn clone_event(&self) -> Arc<dyn ApplicationEvent>;
}

impl<T> ApplicationEvent for T
where
    T: Any + Clone + Send + Sync + fmt::Debug + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn clone_event(&self) -> Arc<dyn ApplicationEvent> {
        Arc::new(self.clone())
    }
}

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
/// # `Any` 使用指引
/// - `ApplicationSpecific` 保留 `Arc<dyn ApplicationEvent>`，以 Marker Trait 包装 `Any` 能力，指导调用方通过 `downcast_application_event` 获取具体类型。
/// - 推荐在扩展事件定义中提供稳定的 `event_kind` 字符串，并在消费方遇到 `None` 或 `downcast` 失败时记录该标识以便排查。
/// - 若某些事件需要零成本访问，可在应用内部单独传递强类型通道，并仅在跨组件边界时包装为 `CoreUserEvent`。
///
/// # 风险提示（Trade-offs）
/// - `ApplicationSpecific` 变体提供开放扩展，但需在上层维护良好命名空间以避免冲突。
///
/// # 示例
/// ```
/// use spark_core::observability::CoreUserEvent;
///
/// #[derive(Clone, Debug)]
/// struct TenantReady;
///
/// let event = CoreUserEvent::from_application_event(TenantReady);
/// assert!(event
///     .downcast_application_event::<TenantReady>()
///     .is_some());
/// ```
#[derive(Debug, Clone)]
pub enum CoreUserEvent {
    TlsEstablished(TlsInfo),
    IdleTimeout(IdleTimeout),
    RateLimited(RateLimited),
    ConfigChanged { keys: Vec<String> },
    ApplicationSpecific(Arc<dyn ApplicationEvent>),
}

impl CoreUserEvent {
    /// 使用类型安全的扩展事件构造 `ApplicationSpecific` 变体。
    ///
    /// # 设计背景（Why）
    /// - 提供对称于 [`crate::buffer::PipelineMessage::from_user`] 的便捷入口，确保调用方显式依赖 [`ApplicationEvent`] 契约。
    /// - 鼓励业务在扩展事件时提供稳定的 `event_kind`，提高审计与运维流程的可观测性。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：`event` 任意实现 [`ApplicationEvent`] 的对象；若依赖默认实现，则需为类型实现 `Clone` 以支撑多路广播。
    /// - **返回值**：封装后的 [`CoreUserEvent::ApplicationSpecific`]，可在 Pipeline Handler、Controller 中分发。
    /// - **前置条件**：建议调用前在团队内定义事件命名规范（如 `team.domain.action`），避免命名冲突。
    /// - **后置条件**：返回事件可多次克隆广播，每次克隆均通过 [`ApplicationEvent::clone_event`] 完成。
    pub fn from_application_event<E>(event: E) -> Self
    where
        E: ApplicationEvent,
    {
        Self::ApplicationSpecific(event.clone_event())
    }

    /// 查询扩展事件的 `event_kind` 标签。
    pub fn application_event_kind(&self) -> Option<&'static str> {
        match self {
            CoreUserEvent::ApplicationSpecific(event) => Some(event.event_kind()),
            _ => None,
        }
    }

    /// 尝试以引用形式获取具体的扩展事件类型。
    pub fn downcast_application_event<E>(&self) -> Option<&E>
    where
        E: ApplicationEvent,
    {
        match self {
            CoreUserEvent::ApplicationSpecific(event) => event.as_any().downcast_ref::<E>(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 示例扩展事件，用于验证类型安全工具函数。
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SampleEvent {
        code: u32,
    }

    #[test]
    fn application_event_helpers_work() {
        let event = SampleEvent { code: 42 };
        let user_event = CoreUserEvent::from_application_event(event);
        assert_eq!(
            user_event.application_event_kind(),
            Some(core::any::type_name::<SampleEvent>())
        );
        let typed = user_event
            .downcast_application_event::<SampleEvent>()
            .expect("downcast to sample event");
        assert_eq!(typed.code, 42);
    }
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
/// # 性能契约（Performance Contract）
/// - `subscribe` 返回 [`BoxStream`]，确保事件总线对象安全；每次订阅会分配 `Box` 并在轮询时通过虚表调度。
/// - 若事件风暴频繁，可在实现层引入泛型订阅接口或无分配环形缓冲，将对象安全层仅暴露给需要动态装配的消费者。
///
/// # 风险提示（Trade-offs）
/// - 建议实现对订阅者生命周期做监控，避免内存泄漏。
pub trait OpsEventBus: Send + Sync + 'static {
    /// 广播事件。
    fn broadcast(&self, event: OpsEvent);

    /// 订阅事件流。
    fn subscribe(&self) -> BoxStream<'static, OpsEvent>;
}
