use alloc::{borrow::Cow, boxed::Box, vec::Vec};

use super::{ChangeNotification, ConfigKey, ConfigValue, ConfigurationError, ProfileId};

/// 配置源的元数据。
///
/// ### 设计目的（Why）
/// - 记录来源名称、优先级等信息，帮助排查冲突与审计。
/// - 吸收 Envoy xDS 资源版本（resource version）的概念，通过 `version` 字段实现增量对比。
///
/// ### 契约说明（What）
/// - `name`：来源的稳定标识，例如 `file:///etc/app/config.yaml`。
/// - `priority`：数值越大优先级越高，决定合并顺序。
/// - `version`：可选版本号，变更时必须递增或变化。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMetadata {
    pub name: Cow<'static, str>,
    pub priority: u16,
    pub version: Option<Cow<'static, str>>,
}

impl SourceMetadata {
    /// 构造函数，供配置源实现者快速返回元数据。
    pub fn new<N>(name: N, priority: u16, version: Option<Cow<'static, str>>) -> Self
    where
        N: Into<Cow<'static, str>>,
    {
        Self {
            name: name.into(),
            priority,
            version,
        }
    }
}

/// 单个配置层。
///
/// ### 设计目的（Why）
/// - 在 Layer 模型中，每个数据源返回一个逻辑层，便于后续合并时进行优先级排序。
/// - 提供 `metadata` 与 `entries`，与 AWS AppConfig、Consul Watch 的 `items + revision` 对齐。
///
/// ### 契约说明（What）
/// - `entries`：由配置键值对组成。若同一键出现多次，以最后一次为准。
/// - `metadata`：描述该层的来源、版本等信息。
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigurationLayer {
    pub metadata: SourceMetadata,
    pub entries: Vec<(ConfigKey, ConfigValue)>,
}

/// 用于订阅配置变化的令牌。
///
/// ### 设计目的（Why）
/// - 借鉴 gRPC streaming API 的取消语义，提供最小化的取消（cancel）句柄。
///
/// ### 契约说明（What）
/// - `cancel` 必须幂等，可以在任何线程调用。
/// - 调用方需要确保在不再需要监听时立即取消，以释放资源。
pub trait WatchToken: Send + Sync {
    fn cancel(&self);
}

/// 配置源契约。
///
/// ### 设计目的（Why）
/// - 抽象不同后端（文件、环境变量、远程配置中心）的加载与监听能力。
/// - 兼容 Envoy xDS 的请求/响应模型，同时适应 Rust `no_std` 场景。
///
/// ### 逻辑解析（How）
/// - `load`：按 Profile 返回完整配置层列表。
/// - `watch`：返回增量通知的流式接口，可选实现。
///
/// ### 契约说明（What）
/// - **前置条件**：`profile` 必须是源支持的档案，否则返回 `ConfigurationError::Validation`。
/// - **后置条件**：`load` 成功时必须至少返回一个 Layer；若无数据，可返回空向量但须保持元数据一致。
/// - `watch` 返回的 [`WatchToken`] 应确保线程安全。
///
/// ### 设计权衡（Trade-offs）
/// - 使用 `Vec` 而非 `Iterator`，简化 FFI 场景下的跨语言传递。
/// - `watch` 默认返回 `None`，避免对不支持热更新的数据源施加负担。
pub trait ConfigurationSource: Send + Sync {
    /// 返回指定 Profile 的配置层集合。
    fn load(&self, profile: &ProfileId) -> Result<Vec<ConfigurationLayer>, ConfigurationError>;

    /// 订阅增量通知。
    fn watch(
        &self,
        _profile: &ProfileId,
        _callback: Box<dyn ChangeCallback + Send + Sync>,
    ) -> Result<Option<Box<dyn WatchToken>>, ConfigurationError> {
        Ok(None)
    }
}

/// 配置变更回调接口。
///
/// ### 设计目的（Why）
/// - 模仿 gRPC streaming 与 Reactor 模型，将变更推送给观察者。
/// - `no_std` 场景下无法依赖 async/await，因此使用回调契约描述。
///
/// ### 契约说明（What）
/// - `on_change` 应保证快速返回，避免阻塞源内部线程。
/// - 若处理失败，应返回 `ConfigurationError`，由数据源决定是否重试或关闭流。
pub trait ChangeCallback {
    fn on_change(&self, notification: ChangeNotification) -> Result<(), ConfigurationError>;
}
