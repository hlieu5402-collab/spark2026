use alloc::borrow::Cow;
use alloc::vec::Vec;

/// `RouteKind` 描述逻辑路由所属的交互范式。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy/Linkerd 的多协议匹配模型，将路由区分为 RPC、消息、流式、控制面等类别，
///   便于路由器在决策时采用不同的负载均衡与弹性策略。
/// - 该枚举同时为学术界常见的 Intent-Based Networking 场景提供“意图分类”维度，
///   方便策略验证或形式化推理工具进行枚举推导。
///
/// # 契约定义（What）
/// - `Rpc`：面向请求-响应的调用（gRPC、Dubbo）。
/// - `Message`：面向消息的路由（MQTT、NATS、Pulsar）。
/// - `Stream`：面向长链接或持续数据流的路由（QUIC Streams、WebTransport）。
/// - `Control`：管理/控制面操作（配置下发、健康检查）。
/// - `Custom`：保留扩展口，允许宿主定义额外范式。
///   - **命名约定**：推荐使用反向域名或组织前缀（如 `acme.command_stream`），确保跨团队唯一。
///   - **实现责任**：路由器实现若无法识别某个 `Custom` 值，应返回 [`crate::error::codes::ROUTER_VERSION_CONFLICT`]
///     或记录告警，而非静默降级，以便调用方快速定位不兼容的扩展。
///
/// # 使用约束（Pre/Post）
/// - **前置条件**：调用方需基于业务语义选择恰当的枚举项，避免语义混淆。
/// - **后置条件**：路由器可据此向下游传递语义，影响 QoS/重试/节流策略。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RouteKind {
    Rpc,
    Message,
    Stream,
    Control,
    Custom(Cow<'static, str>),
}

/// `RouteSegment` 代表逻辑路由路径中的一个层级。
///
/// # 设计动机（Why）
/// - 参考 gRPC method、HTTP path、NATS subject 的分段模式，将层级化路径统一。
/// - 支持参数化段与通配符，复用 Envoy Route Pattern 与 GraphQL Schema 的思想，
///   以满足多租户、灰度发布等动态匹配需求。
///
/// # 构成要素（What）
/// - `Literal`：固定字符串段，最常见。
/// - `Parameter`：命名占位符，路由时需提取实际值供中间件使用。
/// - `Wildcard`：匹配任意剩余段，适用于兜底策略。
///
/// # 约束与风险（Trade-offs）
/// - 仅允许 ASCII 字符构成，避免不同平台的大小写/编码差异；
///   调用方应在构造前完成校验（出于零开销考虑本契约不强制校验）。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RouteSegment {
    Literal(Cow<'static, str>),
    Parameter(Cow<'static, str>),
    Wildcard,
}

/// `RouteId` 是已经物化（Resolved）的路由标识。
///
/// # 角色定位（Why）
/// - 生产环境中路由表需要可追踪、可度量的稳定 ID；
///   该结构提供不可变视图，用于路由结果、观测数据记录。
/// - 对应学术界的 Policy Identifier，便于与形式化验证工具交互。
///
/// # 结构说明（What）
/// - `kind`：交互范式，影响后续调度。
/// - `segments`：序列化的路径段，均为 `Literal`，保证结果可落地。
///
/// # 前置/后置条件
/// - **前置**：使用 [`RoutePattern`] 进行匹配后才应构造 `RouteId`，确保段均为具体值。
/// - **后置**：可安全序列化/散列，并作为 `RouteBinding` 与 `RouteCatalog` 的主键。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RouteId {
    kind: RouteKind,
    segments: Vec<Cow<'static, str>>,
}

impl RouteId {
    /// 构造函数。
    ///
    /// # 参数契约
    /// - `kind`：路由范式，详见 [`RouteKind`]。
    /// - `segments`：按顺序排列的路径片段，必须全部为字面量。
    ///
    /// # 设计取舍
    /// - 采用 `Vec<Cow<'static, str>>`，既能静态定义（`"inventory"`），也支持运行时动态分配，
    ///   满足云原生与嵌入式场景的统一需求。
    ///
    /// # 边界注意
    /// - 若传入空集合，将表示“根”路由，适用于控制面或默认服务；
    ///   调用方应根据业务确认是否允许该情况。
    pub fn new(kind: RouteKind, segments: Vec<Cow<'static, str>>) -> Self {
        Self { kind, segments }
    }

    /// 返回路由范式。
    pub fn kind(&self) -> &RouteKind {
        &self.kind
    }

    /// 返回路径片段迭代器。
    pub fn segments(&self) -> impl Iterator<Item = &Cow<'static, str>> {
        self.segments.iter()
    }
}

/// `RoutePattern` 用于描述尚未物化的匹配规则。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy Route Matcher、Kubernetes Gateway API，以声明式方式描述路由模式，
///   支持静态路由与基于标签的动态路由共存。
/// - 对应学术界的 Routing Intent，可将其转换为逻辑谓词进行验证。
///
/// # 结构说明（What）
/// - `kind`：目标交互范式。
/// - `segments`：匹配段，可以包含 [`RouteSegment::Parameter`] 与 [`RouteSegment::Wildcard`]。
///
/// # 约束与风险（Trade-offs）
/// - 模式解析属于性能敏感路径，框架实现者可选用基于前缀树或 DFA 的高效匹配结构；
///   本契约不强制具体算法，但建议在注释/文档中披露实现选择。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutePattern {
    kind: RouteKind,
    segments: Vec<RouteSegment>,
}

impl RoutePattern {
    /// 构造匹配模式。
    pub fn new(kind: RouteKind, segments: Vec<RouteSegment>) -> Self {
        Self { kind, segments }
    }

    /// 返回交互范式。
    pub fn kind(&self) -> &RouteKind {
        &self.kind
    }

    /// 返回匹配片段迭代器。
    pub fn segments(&self) -> impl Iterator<Item = &RouteSegment> {
        self.segments.iter()
    }
}
