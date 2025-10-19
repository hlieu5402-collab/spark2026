use alloc::borrow::Cow;
use alloc::vec::Vec;

use super::metadata::RouteMetadata;
use super::route::{RouteId, RoutePattern};

/// `RouteDescriptor` 描述一条可配置的路由条目。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy xDS `RouteConfiguration`、Linkerd Service Profile，将配置项抽象为可枚举的描述，
///   便于控制面列举、观测系统抓取、CLI 工具展示。
/// - 提供 `summary` 字段，用于人类可读说明，强化可运维性。
///
/// # 字段说明（What）
/// - `id`：若该路由已物化，则提供具体 ID；未部署的草稿可为空。
/// - `pattern`：声明式匹配模式。
/// - `metadata`：静态属性集合，用于策略和治理。
/// - `summary`：可选的简要说明。
#[derive(Clone, Debug)]
pub struct RouteDescriptor {
    id: Option<RouteId>,
    pattern: RoutePattern,
    metadata: RouteMetadata,
    summary: Option<Cow<'static, str>>,
}

impl RouteDescriptor {
    /// 创建新的路由描述。
    pub fn new(pattern: RoutePattern) -> Self {
        Self {
            id: None,
            pattern,
            metadata: RouteMetadata::new(),
            summary: None,
        }
    }

    /// 指定已物化的 ID。
    pub fn with_id(mut self, id: RouteId) -> Self {
        self.id = Some(id);
        self
    }

    /// 指定静态属性。
    pub fn with_metadata(mut self, metadata: RouteMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// 指定摘要。
    pub fn with_summary(mut self, summary: Cow<'static, str>) -> Self {
        self.summary = Some(summary);
        self
    }

    /// 获取 ID。
    pub fn id(&self) -> Option<&RouteId> {
        self.id.as_ref()
    }

    /// 获取匹配模式。
    pub fn pattern(&self) -> &RoutePattern {
        &self.pattern
    }

    /// 获取静态属性。
    pub fn metadata(&self) -> &RouteMetadata {
        &self.metadata
    }

    /// 获取摘要。
    pub fn summary(&self) -> Option<&Cow<'static, str>> {
        self.summary.as_ref()
    }
}

/// `RouteCatalog` 聚合路由描述信息，形成只读视图。
///
/// # 设计动机（Why）
/// - 结合 Kubernetes Informer Snapshots、Consul Catalog 的理念，提供稳定、可迭代的列表，
///   以便调用方缓存并用于本地决策或测试。
///
/// # 字段说明（What）
/// - `entries`：路由描述集合。
#[derive(Clone, Debug, Default)]
pub struct RouteCatalog {
    entries: Vec<RouteDescriptor>,
}

impl RouteCatalog {
    /// 创建空目录。
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 追加条目。
    pub fn push(&mut self, descriptor: RouteDescriptor) {
        self.entries.push(descriptor);
    }

    /// 遍历条目。
    pub fn iter(&self) -> impl Iterator<Item = &RouteDescriptor> {
        self.entries.iter()
    }
}
