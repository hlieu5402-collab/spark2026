use crate::Error;
use crate::host::context::HostContext;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// 配置查询请求。
///
/// # 背景（Why）
/// - 融合 Dapr Configuration API、Envoy xDS 订阅与 Consul KV 查询的理念，支持组件根据命名空间与标签获取配置。
///
/// # 字段说明（What）
/// - `namespace`：逻辑命名空间，通常对应环境或租户。
/// - `keys`：请求的配置键集合。
/// - `metadata`：附加过滤条件，例如版本、灰度标签。
///
/// # 契约（Contract）
/// - **前置条件**：调用方至少指定一个键或 metadata 条件；宿主可据此决定返回范围。
/// - **后置条件**：宿主应返回一个 `ConfigEnvelope`，即使没有命中任何配置，也应返回空集合而非错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigQuery {
    /// 配置命名空间。
    pub namespace: String,
    /// 目标键集合。
    pub keys: Vec<String>,
    /// 附加元数据过滤。
    pub metadata: BTreeMap<String, String>,
}

impl ConfigQuery {
    /// 快速构造仅包含命名空间的查询。
    pub fn namespace(namespace: String) -> Self {
        Self {
            namespace,
            keys: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }
}

/// 配置包络，用于携带版本化的配置数据。
///
/// # 背景（Why）
/// - 借鉴 Kubernetes ConfigMap、HashiCorp Vault Secret 版本管理的实践，允许组件基于版本判断是否需要刷新。
///
/// # 字段说明（What）
/// - `namespace`：配置所属命名空间。
/// - `items`：键值对集合。
/// - `version`：宿主生成的版本或校验和。
/// - `metadata`：额外信息，如签名、发布时间。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigEnvelope {
    /// 命名空间。
    pub namespace: String,
    /// 配置项。
    pub items: BTreeMap<String, String>,
    /// 版本信息。
    pub version: Option<String>,
    /// 附加元数据。
    pub metadata: BTreeMap<String, String>,
}

impl ConfigEnvelope {
    /// 创建空的配置包络。
    pub fn empty(namespace: String) -> Self {
        Self {
            namespace,
            items: BTreeMap::new(),
            version: None,
            metadata: BTreeMap::new(),
        }
    }
}

/// 配置变更事件。
///
/// # 背景（Why）
/// - 对齐 Envoy Delta xDS 与 Dapr Subscribe 模型，明确变更触发时的增量信息。
///
/// # 字段说明（What）
/// - `envelope`：当前生效的配置快照。
/// - `changed_keys`：变更的键集合，便于组件做局部刷新。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigChange {
    /// 最新配置快照。
    pub envelope: ConfigEnvelope,
    /// 发生变更的键。
    pub changed_keys: Vec<String>,
}

impl ConfigChange {
    /// 根据快照和变更键构造事件。
    pub fn new(envelope: ConfigEnvelope, changed_keys: Vec<String>) -> Self {
        Self {
            envelope,
            changed_keys,
        }
    }
}

/// 配置下发的结果。
///
/// # 背景（Why）
/// - 宿主在推送配置时需要收集组件反馈，借鉴 Istio Pilot、Envoy ACK/NACK 模型，提供明确的确认语义。
///
/// # 枚举说明（What）
/// - `Applied`：组件接受并成功应用配置，可附带额外诊断信息。
/// - `Rejected`：组件拒绝配置，必须说明原因，宿主应停止推进。
/// - `Deferred`：组件暂缓应用配置，例如等待依赖加载。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisioningOutcome {
    /// 成功应用配置。
    Applied { details: Option<String> },
    /// 拒绝应用配置。
    Rejected { reason: String },
    /// 延迟应用配置。
    Deferred { retry_after_seconds: Option<u32> },
}

impl ProvisioningOutcome {
    /// 创建一个成功结果。
    pub fn applied(details: Option<String>) -> Self {
        ProvisioningOutcome::Applied { details }
    }

    /// 创建一个拒绝结果。
    pub fn rejected(reason: String) -> Self {
        ProvisioningOutcome::Rejected { reason }
    }

    /// 创建一个延迟结果。
    pub fn deferred(retry_after_seconds: Option<u32>) -> Self {
        ProvisioningOutcome::Deferred {
            retry_after_seconds,
        }
    }
}

/// 组件接收配置时应实现的回调接口。
///
/// # 背景（Why）
/// - 组件可能既要处理初始全量配置，也要处理后续的增量更新，此接口兼容两种场景。
///
/// # 契约（What）
/// - `Error`：处理失败时返回的错误类型。
/// - `on_initial_provision`：首次加载配置，通常在组件初始化时调用。
/// - `on_incremental_change`：后续增量变更回调。
///
/// # 前置/后置条件
/// - **前置条件**：宿主保证回调时 `HostContext` 仍然有效。
/// - **后置条件**：返回 `ProvisioningOutcome`，宿主据此决定是否继续推送或重试。
pub trait ConfigConsumer {
    /// 错误类型。
    type Error: Error;

    /// 初次下发配置。
    fn on_initial_provision(
        &self,
        ctx: &HostContext,
        config: ConfigEnvelope,
    ) -> Result<ProvisioningOutcome, Self::Error>;

    /// 增量配置变更通知。
    fn on_incremental_change(
        &self,
        ctx: &HostContext,
        change: ConfigChange,
    ) -> Result<ProvisioningOutcome, Self::Error>;
}
