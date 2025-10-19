use crate::{BoxFuture, BoxStream, SparkError, transport::Endpoint};
use alloc::{collections::BTreeMap, string::String, vec::Vec};

/// 集群节点唯一标识。
///
/// # 设计背景（Why）
/// - 集群成员协议（Gossip、K8s API）通常以字符串 ID 表示节点，因此直接使用 `String` 避免额外转换。
///
/// # 契约说明（What）
/// - NodeId 应唯一且稳定，可使用 UUID、主机名或自定义标识。
///
/// # 风险提示（Trade-offs）
/// - 未强制格式校验，调用方需在上层保证合法性。
pub type NodeId = String;

/// 节点状态信息。
///
/// # 设计背景（Why）
/// - 区分节点可用性，便于路由层做降级或切换。
///
/// # 契约说明（What）
/// - `Up` 表示可正常接收流量，`Down` 表示离线，`Suspected` 建议降权，`Leaving` 禁止新请求。
///
/// # 风险提示（Trade-offs）
/// - 状态转换依赖外部系统，若更新滞后可能导致短暂路由不一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeStatus {
    /// 节点正常在线。
    Up,
    /// 节点离线，不应发送新流量。
    Down,
    /// 节点状态存疑，应降权或探活。
    Suspected,
    /// 节点即将退出集群。
    Leaving,
}

/// 节点详细信息。
///
/// # 设计背景（Why）
/// - 汇集节点元信息，供负载均衡、可观测性与 AIOps 使用。
///
/// # 契约说明（What）
/// - `endpoint` 指定默认通信地址，`metadata` 可包含权重、区域等键值。
///
/// # 风险提示（Trade-offs）
/// - 为保持 `no_std`，未提供复杂结构化类型；实现者可在元数据中存 JSON 或 Bincode。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    pub node_id: NodeId,
    pub endpoint: Endpoint,
    pub metadata: BTreeMap<String, String>,
    pub status: NodeStatus,
}

/// 成员变更事件。
///
/// # 设计背景（Why）
/// - 以流式事件形式通知路由层，减少全量轮询成本。
///
/// # 契约说明（What）
/// - 事件需按时间顺序发送；`MemberStatusChanged` 用于显式状态更新。
///
/// # 风险提示（Trade-offs）
/// - 若事件丢失，调用方应结合 `get_members` 快照进行自我修复。
#[derive(Clone, Debug)]
pub enum MembershipEvent {
    MemberUp(NodeInfo),
    MemberDown(NodeId),
    MemberUpdated(NodeInfo),
    MemberStatusChanged {
        node_id: NodeId,
        new_status: NodeStatus,
    },
}

/// 集群成员信息提供者。
///
/// # 设计背景（Why）
/// - 抽象 Gossip、Consul、K8s 等不同实现，统一对外接口。
///
/// # 契约说明（What）
/// - `subscribe` 必须提供有界缓冲，并在丢弃事件时暴露指标。
/// - `get_members` / `get_self_info` 返回 `BoxFuture`，方便异步实现。
///
/// # 风险提示（Trade-offs）
/// - 如底层一致性保证较弱，调用方应结合多副本校验结果。
pub trait ClusterMembershipProvider: Send + Sync + 'static {
    /// 订阅成员事件。
    fn subscribe(&self) -> BoxStream<'static, MembershipEvent>;

    /// 获取当前成员列表。
    fn get_members(&self) -> BoxFuture<'static, Result<Vec<NodeInfo>, SparkError>>;

    /// 获取自身节点信息。
    fn get_self_info(&self) -> BoxFuture<'static, Result<NodeInfo, SparkError>>;
}

/// 服务发现事件。
///
/// # 设计背景（Why）
/// - 支持对逻辑服务的实例增删进行流式通知，与负载均衡器联动。
///
/// # 契约说明（What）
/// - 事件包含对应 `Endpoint`，若同一实例多次更新应发送 `InstanceUpdated`。
///
/// # 风险提示（Trade-offs）
/// - 服务注册中心短暂抖动时可能频繁触发事件，调用方应在上层去抖。
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    InstanceUp(Endpoint),
    InstanceDown(Endpoint),
    InstanceUpdated(Endpoint),
}

/// 服务发现提供者。
///
/// # 设计背景（Why）
/// - 统一 DNS、Consul、K8s Service 等发现机制。
///
/// # 契约说明（What）
/// - `resolve` 提供一次性解析，`subscribe` 提供增量事件，两者应共享缓存策略。
///
/// # 风险提示（Trade-offs）
/// - 若网络分区导致解析失败，应返回 `SparkError` 并附带可观测性信息。
pub trait ServiceDiscoveryProvider: Send + Sync + 'static {
    /// 解析服务名。
    fn resolve(&self, service_name: &str) -> BoxFuture<'static, Result<Vec<Endpoint>, SparkError>>;

    /// 订阅服务实例变更。
    fn subscribe(&self, service_name: &str) -> BoxStream<'static, DiscoveryEvent>;
}
