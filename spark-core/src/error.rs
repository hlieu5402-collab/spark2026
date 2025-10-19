use crate::{Error, TraceContext, cluster::NodeId, transport::SparkSocketAddr};
use alloc::{boxed::Box, string::String};
use core::fmt;

/// `SparkError` 表示 `spark-core` 统一的错误域。
///
/// # 设计背景（Why）
/// - 框架需要跨层传递稳定的错误码，以便日志、指标与 AIOps 系统能够进行机器可读的根因识别。
/// - 错误必须运行在 `no_std` 环境下，因此不依赖 `std::error::Error`，并兼容可选的链路上下文（链路追踪、节点信息等）。
///
/// # 逻辑解析（How）
/// - 结构体以 Builder 风格的方法累积上下文，例如 `with_cause`、`with_trace`（见下方实现）。
/// - `code` 字段承载稳定错误码，`message` 面向人类调试；其余字段为可选元信息，帮助运行时进行降噪或溯源。
///
/// # 契约说明（What）
/// - **前置条件**：调用方应保证错误码在 `codes` 模块中声明，或遵守约定的 `namespace.action` 形式。
/// - **后置条件**：所有构造方法都会产生 `SparkError` 拥有的所有权，确保可以跨线程移动与重试。
///
/// # 设计取舍与风险（Trade-offs）
/// - 采用 `String` 储存消息，牺牲少量拷贝成本换取在日志与跨组件通信上的灵活性。
/// - `TraceContext`、节点信息均为可选，以免在轻量场景（如单机部署）产生多余依赖。
#[derive(Debug)]
pub struct SparkError {
    code: &'static str,
    message: String,
    cause: Option<ErrorCause>,
    trace_context: Option<TraceContext>,
    peer_addr: Option<SparkSocketAddr>,
    node_id: Option<NodeId>,
}

/// `ErrorCause` 封装底层原因，保持 `Send + Sync` 以方便跨线程传递。
pub type ErrorCause = Box<dyn Error + Send + Sync + 'static>;

impl SparkError {
    /// 使用稳定错误码与消息创建 `SparkError`。
    ///
    /// # 契约说明
    /// - **参数**：`code` 必须是全局唯一且稳定的字符串；`message` 为任意人类可读文本。
    /// - **前置条件**：`code` 应遵循 `domain.reason` 命名；`message` 建议避免敏感信息。
    /// - **后置条件**：返回的实例尚未附带任何补充上下文。
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            cause: None,
            trace_context: None,
            peer_addr: None,
            node_id: None,
        }
    }

    /// 获取稳定错误码。
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// 获取人类可读的错误描述。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 附带一个底层原因，形成错误链。
    pub fn with_cause(mut self, cause: impl Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// 附带链路追踪信息，便于分布式追踪。
    pub fn with_trace(mut self, trace_context: TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    /// 附带通信对端地址，帮助定位网络问题。
    pub fn with_peer_addr(mut self, peer: SparkSocketAddr) -> Self {
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
    pub fn peer_addr(&self) -> Option<&SparkSocketAddr> {
        self.peer_addr.as_ref()
    }

    /// 获取可选的节点 ID。
    pub fn node_id(&self) -> Option<&NodeId> {
        self.node_id.as_ref()
    }

    /// 获取可选的底层原因。
    pub fn cause(&self) -> Option<&ErrorCause> {
        self.cause.as_ref()
    }
}

impl fmt::Display for SparkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl Error for SparkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn Error + 'static))
    }
}

/// 框架内置的错误码常量集合，确保可观测性系统具有稳定识别符。
pub mod codes {
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
    /// 应用路由失败。
    pub const APP_ROUTING_FAILED: &str = "app.routing_failed";
    /// 应用鉴权失败。
    pub const APP_UNAUTHORIZED: &str = "app.unauthorized";
    /// 应用背压施加。
    pub const APP_BACKPRESSURE_APPLIED: &str = "app.backpressure_applied";
}
