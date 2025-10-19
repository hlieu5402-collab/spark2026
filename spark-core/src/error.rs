use crate::{Error, TraceContext, cluster::NodeId, transport::TransportSocketAddr};
use alloc::{borrow::Cow, boxed::Box, string::String};
use core::fmt;

/// `CoreError` 提供稳定的错误码与根因链路，是框架错误分层的最底层。
///
/// # 设计背景（Why）
/// - 错误分层共识要求“核心错误可稳定 round-trip”，因此该结构仅承载错误码、消息与底层 `source`。
/// - 通过对象安全的 [`Error`] 实现，保证在 `no_std + alloc` 环境下同样可用。
///
/// # 契约说明（What）
/// - `code`：稳定字符串，建议使用 `namespace.reason` 命名规范。
/// - `message`：人类可读描述，避免包含敏感信息。
/// - `cause`：可选底层原因；若不存在可设为 `None`。
///
/// # 风险提示（Trade-offs）
/// - 结构体仅负责承载信息，不执行任何格式化或指标上报逻辑；调用方需自行处理。
#[derive(Debug)]
pub struct CoreError {
    code: &'static str,
    message: Cow<'static, str>,
    cause: Option<ErrorCause>,
}

impl CoreError {
    /// 构造核心错误。
    pub fn new(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code,
            message: message.into(),
            cause: None,
        }
    }

    /// 附带底层原因并返回新的核心错误。
    pub fn with_cause(mut self, cause: impl Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// 为现有错误设置底层原因。
    pub fn set_cause(&mut self, cause: impl Error + Send + Sync + 'static) {
        self.cause = Some(Box::new(cause));
    }

    /// 获取稳定错误码。
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// 获取描述。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 获取底层原因。
    pub fn cause(&self) -> Option<&ErrorCause> {
        self.cause.as_ref()
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl Error for CoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn Error + 'static))
    }
}

/// `SparkError`（DomainError）在核心错误之上附加分布式上下文信息。
///
/// # 设计背景（Why）
/// - 结合 Trace/Peer/Node 元信息，满足跨节点调试、审计与 AIOps 需求。
/// - `source()` 始终返回内部的 [`CoreError`]，以保证错误链 `Impl → Domain → Core → Cause` 的 round-trip。
///
/// # 契约说明（What）
/// - 所有 Builder 方法返回 `Self`，保持不可变语义，避免多线程情况下出现部分更新。
/// - `with_cause` 实际调用核心错误的 `with_cause`，确保底层 cause 与核心层共享。
#[derive(Debug)]
pub struct SparkError {
    core: CoreError,
    trace_context: Option<TraceContext>,
    peer_addr: Option<TransportSocketAddr>,
    node_id: Option<NodeId>,
}

/// `ErrorCause` 封装底层原因，保持 `Send + Sync` 以方便跨线程传递。
pub type ErrorCause = Box<dyn Error + Send + Sync + 'static>;

impl SparkError {
    /// 使用稳定错误码与消息创建 `SparkError`。
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            core: CoreError::new(code, Cow::Owned(message.into())),
            trace_context: None,
            peer_addr: None,
            node_id: None,
        }
    }

    /// 获取稳定错误码。
    pub fn code(&self) -> &'static str {
        self.core.code()
    }

    /// 获取人类可读的错误描述。
    pub fn message(&self) -> &str {
        self.core.message()
    }

    /// 附带一个底层原因，形成错误链。
    pub fn with_cause(mut self, cause: impl Error + Send + Sync + 'static) -> Self {
        self.core.set_cause(cause);
        self
    }

    /// 附带链路追踪信息，便于分布式追踪。
    pub fn with_trace(mut self, trace_context: TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    /// 附带通信对端地址，帮助定位网络问题。
    pub fn with_peer_addr(mut self, peer: TransportSocketAddr) -> Self {
        self.peer_addr = Some(peer);
        self
    }

    /// 附带节点 ID，适配分布式部署环境。
    pub fn with_node_id(mut self, node: NodeId) -> Self {
        self.node_id = Some(node);
        self
    }

    /// 获取可选的链路追踪信息。
    pub fn trace_context(&self) -> Option<&TraceContext> {
        self.trace_context.as_ref()
    }

    /// 获取可选的对端地址。
    pub fn peer_addr(&self) -> Option<&TransportSocketAddr> {
        self.peer_addr.as_ref()
    }

    /// 获取可选的节点 ID。
    pub fn node_id(&self) -> Option<&NodeId> {
        self.node_id.as_ref()
    }

    /// 获取可选的底层原因。
    pub fn cause(&self) -> Option<&ErrorCause> {
        self.core.cause()
    }

    /// 访问核心错误。
    pub fn core(&self) -> &CoreError {
        &self.core
    }
}

impl fmt::Display for SparkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.core)
    }
}

impl Error for SparkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.core() as &dyn Error)
    }
}

/// `ImplError`（Implementation Error）包装领域错误并附加实现细节。
///
/// # 设计背景（Why）
/// - 满足分层共识中“实现细节可扩展”的要求，便于在不同宿主/协议实现中记录私有信息。
///
/// # 契约说明（What）
/// - `detail`：实现层描述，推荐使用稳定字符串便于日志聚合。
/// - `source_cause`：实现层额外的底层错误，例如系统调用或第三方库异常。
#[derive(Debug)]
pub struct ImplError {
    domain: SparkError,
    detail: Option<Cow<'static, str>>,
    source_cause: Option<ErrorCause>,
}

impl ImplError {
    /// 使用领域错误创建实现错误。
    pub fn new(domain: SparkError) -> Self {
        Self {
            domain,
            detail: None,
            source_cause: None,
        }
    }

    /// 附加实现细节描述。
    pub fn with_detail(mut self, detail: impl Into<Cow<'static, str>>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// 附加底层错误原因。
    pub fn with_source(mut self, cause: impl Error + Send + Sync + 'static) -> Self {
        self.source_cause = Some(Box::new(cause));
        self
    }

    /// 获取领域错误。
    pub fn domain(&self) -> &SparkError {
        &self.domain
    }

    /// 消耗自身并返回领域错误。
    pub fn into_domain(self) -> SparkError {
        self.domain
    }

    /// 获取实现细节描述。
    pub fn detail(&self) -> Option<&Cow<'static, str>> {
        self.detail.as_ref()
    }
}

impl fmt::Display for ImplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({})", self.domain, detail),
            None => write!(f, "{}", self.domain),
        }
    }
}

impl Error for ImplError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Some(ref cause) = self.source_cause {
            Some(cause.as_ref())
        } else {
            Some(self.domain() as &dyn Error)
        }
    }
}

/// 框架内置的错误码常量集合，确保可观测性系统具有稳定识别符。
pub mod codes {
    /// 分布式子系统的关键错误码集合。
    ///
    /// # 设计背景（Why）
    /// - 与评审阶段的共识保持一致：网络分区、领导者丢失、事件队列溢出、陈旧读取、版本冲突
    ///   是分布式系统面临的高频故障模式，必须提供标准化标识以便调用方实施兜底策略。
    /// - 错误码遵循 `<领域>.<语义>` 命名约定，方便在跨组件日志中检索与聚合。
    ///
    /// # 契约说明（What）
    /// - **使用前提**：错误码应由实现者封装进 [`SparkError`](crate::SparkError) 或下游错误类型，
    ///   并确保在链路日志、度量中携带完整上下文。
    /// - **返回承诺**：调用方收到这些错误码后，可据此触发补救措施（如退避、刷新快照、重试或
    ///   请求人工干预）。
    ///
    /// # 设计取舍与风险（Trade-offs）
    /// - 保持错误码粒度适中：既能覆盖分布式一致性与拓扑相关问题，又避免枚举过细导致实现者
    ///   难以判定场景。若后续引入更细分语义（如“复制日志滞后”），需确保与现有码值兼容。
    ///
    /// 传输层 I/O 错误。
    pub const TRANSPORT_IO: &str = "transport.io";
    /// 传输层超时。
    pub const TRANSPORT_TIMEOUT: &str = "transport.timeout";
    /// 协议解码失败。
    pub const PROTOCOL_DECODE: &str = "protocol.decode";
    /// 协议内容协商失败。
    pub const PROTOCOL_NEGOTIATION: &str = "protocol.negotiation";
    /// 协议预算超限。
    pub const PROTOCOL_BUDGET_EXCEEDED: &str = "protocol.budget_exceeded";
    /// 编解码类型不匹配。
    pub const PROTOCOL_TYPE_MISMATCH: &str = "protocol.type_mismatch";
    /// 运行时关闭。
    pub const RUNTIME_SHUTDOWN: &str = "runtime.shutdown";
    /// 集群节点不可用。
    pub const CLUSTER_NODE_UNAVAILABLE: &str = "cluster.node_unavailable";
    /// 集群未找到服务。
    pub const CLUSTER_SERVICE_NOT_FOUND: &str = "cluster.service_not_found";
    /// 集群控制面发生网络分区。
    pub const CLUSTER_NETWORK_PARTITION: &str = "cluster.network_partition";
    /// 集群当前领导者丢失（例如选举中）。
    pub const CLUSTER_LEADER_LOST: &str = "cluster.leader_lost";
    /// 集群内部事件队列溢出。
    pub const CLUSTER_QUEUE_OVERFLOW: &str = "cluster.queue_overflow";
    /// 服务发现返回陈旧数据。
    pub const DISCOVERY_STALE_READ: &str = "discovery.stale_read";
    /// 路由元数据版本冲突。
    pub const ROUTER_VERSION_CONFLICT: &str = "router.version_conflict";
    /// 应用路由失败。
    pub const APP_ROUTING_FAILED: &str = "app.routing_failed";
    /// 应用鉴权失败。
    pub const APP_UNAUTHORIZED: &str = "app.unauthorized";
    /// 应用背压施加。
    pub const APP_BACKPRESSURE_APPLIED: &str = "app.backpressure_applied";
}
