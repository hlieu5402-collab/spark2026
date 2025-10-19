use alloc::string::String;
use alloc::vec::Vec;

/// 宿主能力声明，用于运行时协商底层通信与安全特性。
///
/// # 背景阐述（Why）
/// - 借鉴 Envoy xDS `Node` 能力协商、gRPC `Channel` 参数以及 NATS `CONNECT` 协议中的可选能力字段。
/// - 在多宿主场景下，组件只有感知宿主真实能力，才能在 QUIC、HTTP/3、mTLS 等协议之间做出兼容选择。
///
/// # 核心结构（What）
/// - `protocols`：宿主支持的 L7/L4 协议清单，遵循主流行业术语。
/// - `address_families`：网络寻址族，决定部署在容器、边缘或内核态时的寻址策略。
/// - `security`：安全特性与支持等级的配对，允许组件快速决策是否启用互认证、密钥轮换等能力。
/// - `max_concurrent_streams`：宿主愿意开放的最大并发流数量，参考 HTTP/2 SETTINGS 与 QUIC 连接特性。
/// - `throughput`：宿主针对性能的整体调优偏好，用于在限流或批处理策略之间选择。
/// - `notes`：保留字段，鼓励宿主通过“约定大于配置”的方式传达实验性能力。
///
/// # 前置/后置条件（Contract）
/// - **前置条件**：宿主在初始化阶段必须提供一份不可变的能力描述，供组件缓存。
/// - **后置条件**：组件读取该结构时不得修改内部集合，建议通过克隆或借用实现防御式复制。
///
/// # 风险与权衡（Trade-offs）
/// - 未强制约束协议枚举为封闭集，允许通过 `Custom` 变体扩展；换来的是调用方需处理未知协议的额外分支。
/// - 并发流的限制采用 `Option`，以兼容 UDP 或消息队列等无概念的场景。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityDescriptor {
    /// 支持的协议集合。
    pub protocols: Vec<NetworkProtocol>,
    /// 支持的寻址族集合。
    pub address_families: Vec<NetworkAddressFamily>,
    /// 安全特性与支持等级配对。
    pub security: Vec<(SecurityFeature, CapabilityLevel)>,
    /// 宿主限制的最大并发流数量。
    pub max_concurrent_streams: Option<u32>,
    /// 宿主对吞吐与延迟的优化倾向。
    pub throughput: ThroughputClass,
    /// 用于传递额外说明的可选文本。
    pub notes: Option<String>,
}

impl CapabilityDescriptor {
    /// 创建一个基础能力描述，简化宿主实现。
    ///
    /// # 设计说明（Why）
    /// - 方便宿主在最小可行实现中快速返回“协议 + 寻址 + 安全”三要素。
    ///
    /// # 逻辑拆解（How）
    /// - 直接收集传入的三个向量，并将其余字段设置为默认值，避免宿主需要显式填写。
    ///
    /// # 契约（What）
    /// - `protocols` / `address_families` / `security`：必须按需构造，允许为空。
    /// - 返回值保证 `max_concurrent_streams` 为 `None`，`throughput` 为 `ThroughputClass::Balanced`。
    ///
    /// # 风险提示
    /// - 若宿主遗漏关键协议，将导致组件降级或无法初始化；请在上线前结合契约测试覆盖。
    pub fn minimal(
        protocols: Vec<NetworkProtocol>,
        address_families: Vec<NetworkAddressFamily>,
        security: Vec<(SecurityFeature, CapabilityLevel)>,
    ) -> Self {
        Self {
            protocols,
            address_families,
            security,
            max_concurrent_streams: None,
            throughput: ThroughputClass::Balanced,
            notes: None,
        }
    }
}

/// 宿主支持协议的行业共识枚举。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkProtocol {
    /// gRPC/HTTP2 语义。
    Grpc,
    /// HTTP/3 或 QUIC 流式语义。
    Http3,
    /// WebSocket 全双工通道。
    WebSocket,
    /// NATS JetStream 等消息队列协议。
    MessageStream,
    /// 基于 QUIC 的自定义协议。
    Quic,
    /// 宿主自行扩展的协议。
    Custom(String),
}

/// 网络寻址族，兼容容器、边缘和本地部署。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkAddressFamily {
    /// IPv4 地址。
    Ipv4,
    /// IPv6 地址。
    Ipv6,
    /// Unix Domain Socket。
    UnixDomain,
    /// 平台扩展。
    Custom(String),
}

/// 安全特性的行业共识枚举。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecurityFeature {
    /// mTLS 双向认证。
    MutualTls,
    /// JWT 或 OIDC 令牌校验。
    JsonWebToken,
    /// API Key 校验。
    ApiKey,
    /// SPIFFE/SPIRE 身份引导。
    WorkloadIdentity,
    /// 可信执行环境。
    TrustedExecution,
    /// 可扩展特性。
    Custom(String),
}

/// 能力支持等级。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityLevel {
    /// 未支持。
    Unsupported,
    /// 需要额外配置或存在限制。
    Limited,
    /// 完全支持。
    Full,
}

/// 宿主的吞吐/延迟偏好枚举。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThroughputClass {
    /// 更关注端到端延迟。
    LatencyOptimized,
    /// 延迟吞吐均衡。
    Balanced,
    /// 偏向批量吞吐。
    ThroughputOptimized,
}
