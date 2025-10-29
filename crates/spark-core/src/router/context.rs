use crate::observability::TraceContext;
use crate::transport::{ConnectionIntent, QualityOfService, SecurityMode};

use super::catalog::RouteCatalog;
use super::metadata::RouteMetadata;
use super::route::{RouteKind, RoutePattern};

/// `RoutingIntent` 描述一次路由判定所携带的高层目标。
///
/// # 设计动机（Why）
/// - 吸收 Intent-Based Networking 与 Kubernetes Gateway 的理念，将“我希望访问什么”与
///   “我期望的服务特性”进行解耦，便于策略引擎独立演进。
/// - 结合生产经验，意图中保留 QoS、安全等参数，满足灰度、敏感流量等需求。
///
/// # 字段说明（What）
/// - `target`：目标路由模式，描述服务名/操作名等层级结构。
/// - `preferred_metadata`：请求方建议附带的属性，如租户、地域、权重偏好。
/// - `expected_qos`：期望的通信质量等级，若为 `None` 则遵循路由默认值。
/// - `security_preference`：安全模式偏好。
///
/// # 契约约束
/// - **前置条件**：调用方需确保 `target` 至少包含一段，或在 `RouteKind::Control` 场景下明确允许空段。
/// - **后置条件**：路由器应在决策结果中保留这些意图，供下游传输与观测组件继续使用。
#[derive(Clone, Debug)]
pub struct RoutingIntent {
    target: RoutePattern,
    preferred_metadata: RouteMetadata,
    expected_qos: Option<QualityOfService>,
    security_preference: Option<SecurityMode>,
}

impl RoutingIntent {
    /// 创建新的路由意图。
    pub fn new(target: RoutePattern) -> Self {
        Self {
            target,
            preferred_metadata: RouteMetadata::new(),
            expected_qos: None,
            security_preference: None,
        }
    }

    /// 设置附加属性。
    pub fn with_metadata(mut self, metadata: RouteMetadata) -> Self {
        self.preferred_metadata = metadata;
        self
    }

    /// 指定期望的 QoS。
    pub fn with_expected_qos(mut self, qos: QualityOfService) -> Self {
        self.expected_qos = Some(qos);
        self
    }

    /// 指定安全偏好。
    pub fn with_security_preference(mut self, security: SecurityMode) -> Self {
        self.security_preference = Some(security);
        self
    }

    /// 读取目标模式。
    pub fn target(&self) -> &RoutePattern {
        &self.target
    }

    /// 访问附加属性。
    pub fn preferred_metadata(&self) -> &RouteMetadata {
        &self.preferred_metadata
    }

    /// 访问 QoS 偏好。
    pub fn expected_qos(&self) -> Option<QualityOfService> {
        self.expected_qos
    }

    /// 访问安全偏好。
    pub fn security_preference(&self) -> Option<&SecurityMode> {
        self.security_preference.as_ref()
    }
}

/// `RoutingSnapshot` 提供路由器当前拓扑的只读视图。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy xDS 与 Linkerd Destination Profile，通过只读快照减少多线程竞争，
///   并为观测/调试提供稳定的拓扑图谱。
///
/// # 字段说明（What）
/// - `catalog`：可用路由的目录视图。
/// - `revision`：快照版本号，通常对应配置下发的世代（generation）。
///
/// # 使用提示
/// - 快照可缓存于调用链，避免重复查询；需要刷新时由路由器提供新的实例。
#[derive(Clone, Copy, Debug)]
pub struct RoutingSnapshot<'a> {
    catalog: &'a RouteCatalog,
    revision: u64,
}

impl<'a> RoutingSnapshot<'a> {
    /// 构造快照。
    pub fn new(catalog: &'a RouteCatalog, revision: u64) -> Self {
        Self { catalog, revision }
    }

    /// 获取目录。
    pub fn catalog(&self) -> &'a RouteCatalog {
        self.catalog
    }

    /// 获取版本号。
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// `RoutingContext` 聚合一次路由决策所需的全部输入。
///
/// # 设计动机（Why）
/// - 融合 Envoy `StreamInfo`、gRPC Metadata、NATS subject pattern 的设计，
///   提供请求数据、传输意图、观测上下文的统一入口。
/// - 允许路由器根据不同维度（请求内容、QoS、安全、观测）做出综合判断。
///
/// # 字段说明（What）
/// - `request`：原始请求引用，仅用于读取关键信息。
/// - `intent`：路由意图，描述目标模式与偏好。
/// - `connection`：底层传输意图，便于结合网络维度决定路由。
/// - `trace`：观测追踪上下文，辅助路由级别的可观测采样。
/// - `dynamic_metadata`：运行时补充的标签，如请求头、租户 ID。
/// - `snapshot`：路由表只读视图，协助在决策中进行本地校验。
///
/// # 契约约束
/// - **前置**：`request` 必须在整个路由决策周期内保持有效；
///   调用方需要确保路由完成前不会释放或修改引用的数据。
/// - **后置**：路由器不可修改 `dynamic_metadata`，但可以读取其内容决定策略。
///
/// # 与执行上下文协同
/// - 路由阶段通常在 Pipeline 调用链中早于服务执行，因此会同时收到 [`crate::context::Context`] 与 `RoutingContext`；
///   前者用于读取取消/截止/预算约束，后者补充路由所需的请求、意图与拓扑信息；
/// - 设计上二者互为补集：`Context` 负责“调用能否继续”，`RoutingContext` 负责“应当如何路由”；
///   若异步任务需要跨线程持有路由上下文，应复制必要的请求/意图数据，避免借用超出 `Context` 生命周期。
#[derive(Debug)]
pub struct RoutingContext<'a, Request> {
    request: &'a Request,
    intent: &'a RoutingIntent,
    connection: Option<&'a ConnectionIntent>,
    trace: Option<&'a TraceContext>,
    dynamic_metadata: &'a RouteMetadata,
    snapshot: RoutingSnapshot<'a>,
}

impl<'a, Request> RoutingContext<'a, Request> {
    /// 构造上下文。
    pub fn new(
        request: &'a Request,
        intent: &'a RoutingIntent,
        connection: Option<&'a ConnectionIntent>,
        trace: Option<&'a TraceContext>,
        dynamic_metadata: &'a RouteMetadata,
        snapshot: RoutingSnapshot<'a>,
    ) -> Self {
        Self {
            request,
            intent,
            connection,
            trace,
            dynamic_metadata,
            snapshot,
        }
    }

    /// 访问原始请求。
    pub fn request(&self) -> &'a Request {
        self.request
    }

    /// 访问路由意图。
    pub fn intent(&self) -> &'a RoutingIntent {
        self.intent
    }

    /// 访问传输意图。
    pub fn connection(&self) -> Option<&'a ConnectionIntent> {
        self.connection
    }

    /// 访问追踪上下文。
    pub fn trace(&self) -> Option<&'a TraceContext> {
        self.trace
    }

    /// 访问动态属性。
    pub fn dynamic_metadata(&self) -> &'a RouteMetadata {
        self.dynamic_metadata
    }

    /// 访问路由快照。
    pub fn snapshot(&self) -> RoutingSnapshot<'a> {
        self.snapshot
    }
}

/// 帮助函数：将意图转换为基础路由种类，便于快速分支。
impl RoutingIntent {
    /// 返回意图目标的路由范式。
    pub fn route_kind(&self) -> &RouteKind {
        self.target.kind()
    }
}
