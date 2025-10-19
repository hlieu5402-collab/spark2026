use crate::{
    BoxFuture, BoxStream,
    cluster::topology::{ClusterConsistencyLevel, ClusterRevision},
    transport::Endpoint,
};
use alloc::{collections::BTreeMap, string::String, vec::Vec};

use super::membership::ClusterError;

/// 逻辑服务名称。
///
/// # 设计背景（Why）
/// - 行业中服务名称普遍采用字符串（DNS 名、Service ID、gRPC authority）。
/// - 允许实现遵循多租户命名规范，如 `tenant.service.version`。
///
/// # 契约说明（What）
/// - 必须是非空字符串，推荐使用小写 + `-` 或 `.` 作为分隔。
/// - **前置条件**：上游需保证名称符合平台约定；本契约不强制校验。
/// - **后置条件**：名称原样传递到事件与快照中，便于跨组件协同。
///
/// # 风险提示（Trade-offs）
/// - 名称空间冲突需由控制面解决；若发生冲突，实现可返回 `cluster.service_conflict` 错误码。
pub type ServiceName = String;

/// 服务实例的结构化描述。
///
/// # 设计背景（Why）
/// - 吸收 Envoy、Istio、Linkerd 等平台的元数据设计，保留协议无关的 Endpoint、权重与标签。
/// - 面向研究场景的多维调度，加入 `hints` 字段以承载副本拓扑、能耗等实验性指标。
///
/// # 契约说明（What）
/// - `service`：实例所属服务名。
/// - `endpoint`：对外通信入口，需与 [`Endpoint`] 兼容。
/// - `metadata`：键值元数据，可包含版本、权重、区域、机型等信息。
/// - `hints`：可选提示，遵循“半结构化 JSON”风格的字符串，如 `az=eu-central-1b`。
/// - `revision`：实例最近一次更新的修订号，帮助消费者进行幂等处理。
/// - **前置条件**：构造时应确保 `service` 非空，`metadata` 键名遵循 `snake_case` 或驼峰共识。
/// - **后置条件**：实例在事件流或快照中出现时，应携带同一修订号。
///
/// # BTreeMap 取舍说明
/// - `metadata` 采用 [`BTreeMap`]，保证序列化与迭代顺序稳定，方便做配置 diff 与审计签名。
/// - 若调用方更关注写入性能，可在读写热点路径中将 `metadata` 克隆为 `HashMap` 并缓存；导出时再转换回有序结构，以兼顾性能与确定性。
/// - 未来如需直接暴露 `HashMap` 视图，可通过增设 feature flag 或附加访问器（如 `fn into_hash_map`) 拓展，本版本保持最小契约。
///
/// # 风险提示（Trade-offs）
/// - `hints` 字段的语义由双方协商，若解释不一致可能导致调度策略偏差；建议结合 Schema 注册中心约束格式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceInstance {
    pub service: ServiceName,
    pub endpoint: Endpoint,
    pub metadata: BTreeMap<String, String>,
    pub hints: Vec<String>,
    pub revision: ClusterRevision,
}

/// 服务解析得到的全量视图。
///
/// # 设计背景（Why）
/// - 微服务场景中，调用方通常先获取一次全量实例列表，再通过事件流增量更新。
/// - 引入修订号，支持实现者采用乐观并发控制或基于版本的缓存更新。
///
/// # 契约说明（What）
/// - `service`：当前快照对应的服务名称。
/// - `instances`：按 endpoint 排序的实例列表。
/// - `revision`：快照生成时的全局修订号。
/// - **前置条件**：如果服务不存在，快照应返回空列表，并可能附带 `cluster.service_not_found` 错误在元数据中记录。
/// - **后置条件**：快照与事件结合可保证最终一致，若调用方存量修订号落后需重新获取快照。
///
/// # 风险提示（Trade-offs）
/// - 大型服务可能存在数千个实例，应当结合分页或分区策略控制快照大小。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoverySnapshot {
    pub service: ServiceName,
    pub instances: Vec<ServiceInstance>,
    pub revision: ClusterRevision,
}

/// 服务发现事件流。
///
/// # 设计背景（Why）
/// - 对齐 Consul、Eureka、Nacos 等平台的事件语义，同时引入 `SnapshotApplied` 以便快速恢复状态。
/// - `TopologyHintUpdated` 面向学术界对拓扑感知路由的研究，可选实现。
///
/// # 契约说明（What）
/// - `InstanceAdded`、`InstanceRemoved`、`InstanceUpdated` 分别表示副本增删改。
/// - `SnapshotApplied` 用于断点恢复，事件流应在推送后继续追加增量事件。
/// - `TopologyHintUpdated` 传递拓扑提示，例如分区布局或副本距离矩阵。
/// - **前置条件**：事件修订号必须递增；若出现乱序，应当在实现端重放或生成快照重置。
/// - **后置条件**：消费者按序处理后，可保证与控制面状态一致。
///
/// # 风险提示（Trade-offs）
/// - 当事件消费者过慢时，建议实现者触发背压指标并在必要时推送快照以降低重放成本。
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    SnapshotApplied(DiscoverySnapshot),
    InstanceAdded {
        revision: ClusterRevision,
        instance: ServiceInstance,
    },
    InstanceRemoved {
        revision: ClusterRevision,
        endpoint: Endpoint,
    },
    InstanceUpdated {
        revision: ClusterRevision,
        instance: ServiceInstance,
    },
    TopologyHintUpdated {
        revision: ClusterRevision,
        hints: Vec<String>,
    },
}

/// 服务发现契约。
///
/// # 设计背景（Why）
/// - 生产系统需支持按一致性等级解析服务以及事件订阅，契约抽象借鉴了 Kubernetes Informer、Envoy xDS、Consul Catalog API。
/// - 面向科研探索，引入拓扑提示、断点续传等机制，方便验证多策略调度。
///
/// # 逻辑解析（How）
/// - `resolve`：根据一致性等级返回最新快照，`Linearizable` 应通过读屏障保证；`Eventual` 可使用缓存。
/// - `watch`：提供增量事件流，可指定修订号从断点继续。
/// - `list_services`：可选实现，用于获取命名空间下的全部服务。
///
/// # 契约说明（What）
/// - **输入参数**：
///   - `service`：目标服务名。
///   - `consistency`：期待的一致性等级。
///   - `resume_from`：事件流起始修订号，`None` 表示从最新状态开始。
/// - **返回值**：
///   - `resolve` 返回 [`DiscoverySnapshot`]。
///   - `watch` 返回 `BoxStream<'static, DiscoveryEvent>`。
///   - `list_services` 返回服务名列表，按字典序排序。
/// - **前置条件**：实现方需在初始化阶段完成注册中心连接，确保契约调用时具备基础数据。
/// - **后置条件**：消费者可将快照与事件结合用于本地缓存或负载均衡决策。
///
/// # 性能契约（Performance Contract）
/// - `resolve` 与 `list_services` 返回 [`BoxFuture`]，`watch` 返回 [`BoxStream`]，用于维持对象安全；每次调用/轮询都会产生堆分配
///   与虚表跳转。
/// - 对于热路径订阅，可在实现中提供额外的泛型 API（例如 `fn watch_typed<T: Stream<...>>`) 或复用内部缓冲，降低 `Box` 分配频率。
/// - 调用方若能接受与具体实现耦合，可直接依赖实现类型并获取零分配的 Stream，实现性能与灵活度的权衡。
///
/// # 设计取舍与风险（Trade-offs）
/// - 解析强一致性需牺牲一定延迟，应结合调用场景（配置下发 vs 实时路由）选择合适等级。
/// - `list_services` 可能导致全量扫描，应在实现中加入分页或速率限制。
pub trait ServiceDiscovery: Send + Sync + 'static {
    /// 解析目标服务并返回快照。
    fn resolve(
        &self,
        service: &ServiceName,
        consistency: ClusterConsistencyLevel,
    ) -> BoxFuture<'static, Result<DiscoverySnapshot, ClusterError>>;

    /// 订阅服务实例事件。
    fn watch(
        &self,
        service: &ServiceName,
        scope: ClusterConsistencyLevel,
        resume_from: Option<ClusterRevision>,
    ) -> BoxStream<'static, DiscoveryEvent>;

    /// 列举当前命名空间下的服务列表。
    fn list_services(&self) -> BoxFuture<'static, Result<Vec<ServiceName>, ClusterError>>;
}
