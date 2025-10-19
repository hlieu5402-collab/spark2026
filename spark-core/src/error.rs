use crate::{Error, TraceContext, cluster::NodeId, transport::TransportSocketAddr};
use alloc::{borrow::ToOwned, boxed::Box, string::String};
use core::fmt;

/// `CoreError` 表示 `spark-core` 跨层共享的稳定错误域，是所有可观察错误的最终形态。
///
/// # 设计背景（Why）
/// - 运行时、域服务与实现细节在不同层次产生的故障需要合流为统一的错误码，以便日志、指标与告警系统能够执行精确的自动化治理。
/// - 框架仍需兼容 `no_std + alloc` 场景，因此不直接依赖 `std::error::Error`，而是复用 crate 内部定义的轻量抽象。
///
/// # 逻辑解析（How）
/// - 结构体以 Builder 风格方法叠加上下文信息（链路追踪、对端地址、节点 ID 与底层原因），并通过 `source()` 暴露完整链路。
/// - 错误码 `code` 始终为 `'static` 字符串，承载稳定语义；`message` 面向排障人员；其余字段用于补充机读上下文。
///
/// # 契约说明（What）
/// - **前置条件**：调用方必须使用 [`codes`] 模块或遵循 `<域>.<语义>` 约定的自定义码值；在构造时尚未附带域或实现层原因。
/// - **返回值**：构造函数返回拥有所有权的 `CoreError`，可安全跨线程移动（`Send + Sync + 'static`）。
/// - **后置条件**：除非显式调用 `with_*` 方法，错误不会包含额外上下文；链路上下文与节点信息需由调用方按需添加。
///
/// # 设计取舍与风险（Trade-offs）
/// - 采用 `String` 保存消息，牺牲极少量堆分配换取在日志、跨语言桥接时的灵活性。
/// - 允许附带可选的传输与链路信息，避免在轻量部署中产生不必要的依赖或复制成本。
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

impl CoreError {
    /// 使用稳定错误码与消息创建 `CoreError`。
    ///
    /// # 契约说明
    /// - **参数**：`code` 为 `'static` 字符串，`message` 为人类可读文本（通常来自域层或实现层错误）。
    /// - **前置条件**：调用方需确保 `code` 与域模型约定一致，且 `message` 不泄露敏感信息。
    /// - **后置条件**：返回的实例未携带底层原因或额外上下文，可继续通过 Builder 方法扩充。
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

    /// 获取稳定错误码，供日志聚合或自动化修复策略使用。
    pub fn code(&self) -> &'static str {
        self.core.code()
    }

    /// 获取人类可读的错误描述。
    pub fn message(&self) -> &str {
        self.core.message()
    }

    /// 附带一个底层原因，形成错误链。
    ///
    /// # 设计意图（Why）
    /// - 将域层或实现层错误纳入核心错误链，确保 `source()` 返回的链路可以被统一遍历。
    ///
    /// # 执行方式（How）
    /// - 使用 trait 对象将任意实现 `Error + Send + Sync + 'static` 的类型装箱。
    ///
    /// # 契约（What）
    /// - **前置条件**：`cause` 必须满足线程安全约束。
    /// - **后置条件**：返回新的 `CoreError`，其 `source()` 指向 `cause`。
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

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.core)
    }
}

impl Error for CoreError {
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
    /// - **使用前提**：错误码应由实现者封装进 [`CoreError`](crate::CoreError) 或下游错误类型，
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

/// 表征域层错误的类别，帮助评审者在文档中快速定位责任边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DomainErrorKind {
    /// 传输层或底层 I/O 场景。
    Transport,
    /// 协议与编解码。
    Protocol,
    /// 运行时资源调度与生命周期管理。
    Runtime,
    /// 集群与拓扑管理。
    Cluster,
    /// 服务发现与配置。
    Discovery,
    /// 路由逻辑。
    Router,
    /// 应用或业务回调。
    Application,
    /// 缓冲区与内存池。
    Buffer,
}

/// 表征实现层（Impl）错误的精细分类，用于辅助诊断具体实现细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImplErrorKind {
    /// 对象池或缓冲区容量不足。
    BufferExhausted,
    /// 编解码注册或动态派发失败。
    CodecRegistry,
    /// 底层 I/O 或系统调用失败。
    Io,
    /// 操作超时或等待被取消。
    Timeout,
    /// 状态机违反约束或遇到非法状态。
    StateViolation,
    /// 未分类或暂未归档的实现细节错误。
    Uncategorized,
}

/// 实现层错误，记录最细粒度的故障信息，并负责与域层组装。
///
/// # 设计背景（Why）
/// - 框架中存在大量面向资源、协议的实现细节，直接暴露给核心错误会导致语义混乱；需要显式的 Impl 层来承载这些细节。
///
/// # 逻辑解析（How）
/// - 记录实现分类、短消息、可选的详细说明与底层原因；`source()` 统一回溯到内部的 `cause`。
///
/// # 契约说明（What）
/// - **构造约束**：`message` 面向域层，需描述具体故障；`detail` 可提供面向开发者的额外上下文。
/// - **行为保证**：实现层错误自身 `Send + Sync + 'static`，可安全装箱并跨线程传递。
#[derive(Debug)]
pub struct ImplError {
    kind: ImplErrorKind,
    message: String,
    detail: Option<String>,
    cause: Option<ErrorCause>,
}

impl ImplError {
    /// 创建实现层错误。
    ///
    /// # 契约说明
    /// - **参数**：`kind` 指出实现分类；`message` 描述对域层可见的问题。
    /// - **后置条件**：返回值未附带详细说明或底层原因。
    pub fn new(kind: ImplErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
            cause: None,
        }
    }

    /// 为错误添加开发者可见的详细说明。
    ///
    /// # 设计动机
    /// - 在不暴露给终端用户的前提下，为日志或调试提供额外语境。
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// 附加更底层的错误原因。
    ///
    /// # 契约
    /// - **前置条件**：`cause` 满足线程安全约束。
    /// - **后置条件**：`source()` 返回该原因。
    pub fn with_cause(mut self, cause: impl Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// 返回实现错误的分类。
    pub fn kind(&self) -> ImplErrorKind {
        self.kind
    }

    /// 返回面向域层的错误描述。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 返回额外的详细说明。
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// 返回底层原因。
    pub fn cause(&self) -> Option<&ErrorCause> {
        self.cause.as_ref()
    }
}

impl fmt::Display for ImplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({detail})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl Error for ImplError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn Error + 'static))
    }
}

/// 域层错误，承载面向调用者的语义，并向核心错误映射。
///
/// # 设计背景（Why）
/// - 实现层错误语义过细，不宜直接暴露；域层需在保持语义完整的前提下，为核心错误提供稳定映射。
///
/// # 逻辑解析（How）
/// - 内部持有一个未携带底层原因的 `CoreError`，以及可选的 `ImplError` 原因；通过 [`IntoCoreError`] trait 执行最终映射。
///
/// # 契约说明（What）
/// - **构造约束**：`core` 不应预先设置 `cause`，否则将导致重复包装；调用方需确保 `core.cause()` 为空。
/// - **行为保证**：域层错误的 `source()` 指向实现层错误，保持错误链顺序。
#[derive(Debug)]
pub struct DomainError {
    kind: DomainErrorKind,
    core: CoreError,
    impl_cause: Option<Box<ImplError>>,
}

impl DomainError {
    /// 以域分类与核心错误上下文构造域层错误。
    ///
    /// # 契约说明
    /// - **参数**：`kind` 指出域责任边界；`core` 为尚未附带原因的核心错误。
    /// - **前置条件**：`core.cause()` 必须为空；若已有原因应在实现层处理后再构造域错误。
    /// - **后置条件**：`impl_cause` 默认缺省，可后续通过 [`Self::with_impl_cause`] 添补。
    pub fn new(kind: DomainErrorKind, core: CoreError) -> Self {
        debug_assert!(core.cause().is_none(), "CoreError 不应在域层前携带 cause");
        Self {
            kind,
            core,
            impl_cause: None,
        }
    }

    /// 从实现层错误直接构造域层错误，复用实现层的消息并保持错误链。
    ///
    /// # 契约
    /// - **参数**：`kind` 表示域分类；`code` 指定核心错误码；`impl_error` 为实现层错误。
    /// - **行为**：消息将沿用 `impl_error.message()`，确保 Round-trip 不丢语义。
    pub fn from_impl(kind: DomainErrorKind, code: &'static str, impl_error: ImplError) -> Self {
        let message = impl_error.message().to_owned();
        let core = CoreError::new(code, message);
        Self {
            kind,
            core,
            impl_cause: Some(Box::new(impl_error)),
        }
    }

    /// 附加实现层原因。
    pub fn with_impl_cause(mut self, cause: ImplError) -> Self {
        self.impl_cause = Some(Box::new(cause));
        self
    }

    /// 返回域分类。
    pub fn kind(&self) -> DomainErrorKind {
        self.kind
    }

    /// 访问域层承载的核心错误上下文。
    pub fn core(&self) -> &CoreError {
        &self.core
    }

    /// 访问实现层原因。
    pub fn impl_cause(&self) -> Option<&ImplError> {
        self.impl_cause.as_ref().map(|boxed| boxed.as_ref())
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.core)
    }
}

impl Error for DomainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.impl_cause
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn Error + 'static))
    }
}

/// 定义从域层映射到核心错误的统一接口，避免跳级转换。
pub trait IntoCoreError {
    /// 将当前错误转换为核心错误，保持消息、错误码与链路来源。
    fn into_core_error(self) -> CoreError;
}

impl IntoCoreError for DomainError {
    fn into_core_error(self) -> CoreError {
        match self.impl_cause {
            Some(cause) => self.core.with_cause(*cause),
            None => self.core,
        }
    }
}

/// 定义实现层向域层上浮的映射接口。
pub trait IntoDomainError {
    /// 将实现层错误映射为域层错误，调用者需显式提供域分类与核心错误码，避免层级混淆。
    fn into_domain_error(self, kind: DomainErrorKind, code: &'static str) -> DomainError;
}

impl IntoDomainError for ImplError {
    fn into_domain_error(self, kind: DomainErrorKind, code: &'static str) -> DomainError {
        DomainError::from_impl(kind, code, self)
    }
}

const _: fn() = || {
    fn assert_error_traits<T: Error + Send + Sync + 'static>() {}

    assert_error_traits::<CoreError>();
    assert_error_traits::<DomainError>();
    assert_error_traits::<ImplError>();
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证实现层 → 域层 → 核心错误的 Round-trip，确保语义保持。
    #[test]
    fn impl_to_domain_to_core_roundtrip_preserves_message_and_cause() {
        // 准备一个实现层错误，包含详细信息与底层原因。
        let impl_cause = ImplError::new(ImplErrorKind::BufferExhausted, "buffer depleted")
            .with_detail("pool=ingress")
            .with_cause(CoreError::new("inner.code", "inner message"));

        // 域层将其映射为核心错误码，并保持消息与 cause。
        let domain_error =
            impl_cause.into_domain_error(DomainErrorKind::Buffer, codes::PROTOCOL_BUDGET_EXCEEDED);

        // 提前检查：域层应能访问实现层 cause。
        let cause = domain_error.impl_cause().expect("impl cause must exist");
        assert_eq!(cause.kind(), ImplErrorKind::BufferExhausted);
        assert_eq!(cause.message(), "buffer depleted");

        // 转换到核心错误后，错误码与消息保持一致，source 链仍可回溯。
        let core_error = domain_error.into_core_error();
        assert_eq!(core_error.code(), codes::PROTOCOL_BUDGET_EXCEEDED);
        assert_eq!(core_error.message(), "buffer depleted");

        let current: &dyn Error = &core_error;
        // 第一层 source：实现层错误。
        let first = current
            .source()
            .expect("core error should expose impl error");
        assert_eq!(format!("{}", first), "buffer depleted (pool=ingress)");

        // 第二层 source：实现层中的底层 cause。
        let second = first
            .source()
            .expect("impl error should expose inner cause");
        assert_eq!(format!("{}", second), "[inner.code] inner message");
    }
}
