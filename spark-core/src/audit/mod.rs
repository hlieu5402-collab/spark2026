//! 审计事件 Schema 及其辅助工具集。
//!
//! # 设计目标（Why）
//! - 提供跨组件共享的事件 Schema，使配置、路由等核心资源的变更能够形成不可篡改的哈希链。
//! - 暴露统一的事件记录接口，便于在运行期挂载文件、消息队列或 TSA（Time-Stamping Authority）等实现。
//! - 内置状态哈希工具与内存回放 Recorder，为 `T12｜审计事件 Schema v1.0` 的验收提供可脚本化样例。
//!
//! # 契约概览（What）
//! - [`AuditEventV1`]：事件载荷，涵盖事件 ID、实体信息、动作、前/后状态哈希、操作者、发生时间以及可选 TSA 锚点。
//! - [`AuditRecorder`]：抽象事件写入器；框架在关键资源变更处调用 `record` 推送事件。
//! - [`AuditStateHasher`]：将配置状态映射为稳定哈希值，保证链路完整性检测的一致性。
//! - [`InMemoryAuditRecorder`]：仅在 `std` 环境可用的参考实现，用于测试与本地回放验证。
//!
//! # 风险与注意事项（Trade-offs）
//! - 模块默认依赖 `alloc`，因此在纯 `no_std` 且无分配器的环境下不可用；若需进一步精简，请在上层提供裁剪版本。
//! - 事件链完整性依赖调用方保证所有写入都经过 [`AuditRecorder`]；若 Recorder 忽略错误，链式检测将失效。

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::configuration::{ChangeSet, ConfigKey, ConfigValue};

use sha2::{Digest, Sha256};

/// 描述审计实体的稳定标识。
///
/// ## 设计动机（Why）
/// - 与 Kubernetes Audit、AWS CloudTrail 等系统保持一致，使用 `kind + id` 组合定位资源。
/// - 允许在跨模块复用时补充额外维度（例如租户、区域），通过 `labels` 字段承载。
///
/// ## 契约说明（What）
/// - `kind`：实体类型，例如 `configuration.profile`、`router.route`。
/// - `id`：实体在其命名空间内的唯一标识。
/// - `labels`：用于补充上下文的键值对，调用方需避免写入敏感信息。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntityRef {
    pub kind: Cow<'static, str>,
    pub id: String,
    #[serde(default)]
    pub labels: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

/// 描述触发审计事件的操作者。
///
/// ## 设计动机（Why）
/// - 审计链需要明确“谁做的变更”，以满足合规与追责要求。
/// - 结构化字段便于后续扩展（例如来源 IP、设备指纹）。
///
/// ## 契约说明（What）
/// - `id`：操作者稳定标识，通常为 IAM 用户或服务账号。
/// - `display_name`：面向展示的人类可读名称，可选。
/// - `tenant`：多租户系统中的租户标识，可选。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditActor {
    pub id: Cow<'static, str>,
    #[serde(default)]
    pub display_name: Option<Cow<'static, str>>,
    #[serde(default)]
    pub tenant: Option<Cow<'static, str>>,
}

/// 时间戳权威（TSA）锚点，用于外部可信时间证明。
///
/// ## 设计动机（Why）
/// - 合规场景常要求事件绑定外部可信时间，防止本地时间被篡改。
///
/// ## 契约说明（What）
/// - `provider`：TSA 服务提供方。
/// - `evidence`：原始签名或凭证字符串，通常为 Base64 编码。
/// - `issued_at`：TSA 签发时间的 Unix 秒级时间戳。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TsaEvidence {
    pub provider: Cow<'static, str>,
    pub evidence: String,
    pub issued_at: u64,
}

/// 审计事件 v1.0 的标准载荷。
///
/// ## 设计动机（Why）
/// - 将事件最小字段固化，保障跨模块、跨语言的兼容性。
/// - 哈希链语义 (`state_prev_hash` -> `state_curr_hash`) 让事件缺失可以被快速检测。
///
/// ## 契约说明（What）
/// - `event_id`：唯一事件标识，调用方应保证全局唯一性。
/// - `sequence`：事件发生顺序，可与数据源自增序列对齐。
/// - `entity`：发生变更的对象引用。
/// - `action`：动作名称（如 `apply_changeset`、`rollback`）。
/// - `state_prev_hash` / `state_curr_hash`：前/后状态的 SHA-256 十六进制摘要。
/// - `actor`：操作者信息。
/// - `occurred_at`：事件发生时间（Unix 毫秒）。
/// - `tsa_evidence`：可选的可信时间锚点。
/// - `changes`：脱敏后的配置变更集合，可用于回放。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditEventV1 {
    pub event_id: String,
    pub sequence: u64,
    pub entity: AuditEntityRef,
    pub action: Cow<'static, str>,
    pub state_prev_hash: String,
    pub state_curr_hash: String,
    pub actor: AuditActor,
    pub occurred_at: u64,
    #[serde(default)]
    pub tsa_evidence: Option<TsaEvidence>,
    pub changes: AuditChangeSet,
}

/// 审计事件中用于回放的差异集合。
///
/// ## 设计动机（Why）
/// - 复用配置模块已有的 `ChangeSet` 语义，同时将键和值转换为可序列化结构。
/// - 在保持最小字段的前提下，确保 CLI 工具能够完全复现非脱敏的样例数据。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditChangeSet {
    pub created: Vec<AuditChangeEntry>,
    pub updated: Vec<AuditChangeEntry>,
    pub deleted: Vec<AuditDeletedEntry>,
}

impl AuditChangeSet {
    /// 根据核心配置模块的 [`ChangeSet`] 构建审计差异集合。
    ///
    /// ### 逻辑解析（How）
    /// - 对 `created` 与 `updated`，将键值对映射为 [`AuditChangeEntry`] 并深拷贝，以保证后续序列化不依赖生命周期。
    /// - 对 `deleted`，仅保留键信息，避免意外泄露历史值。
    pub fn from_change_set(changes: &ChangeSet) -> Self {
        let created = changes
            .created
            .iter()
            .map(|(key, value)| AuditChangeEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        let updated = changes
            .updated
            .iter()
            .map(|(key, value)| AuditChangeEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        let deleted = changes
            .deleted
            .iter()
            .map(|key| AuditDeletedEntry { key: key.clone() })
            .collect();
        Self {
            created,
            updated,
            deleted,
        }
    }
}

/// 表示单个新增或更新的配置条目。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditChangeEntry {
    pub key: ConfigKey,
    pub value: ConfigValue,
}

/// 表示单个删除的配置条目。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuditDeletedEntry {
    pub key: ConfigKey,
}

/// Recorder 接口定义。
///
/// ## 设计动机（Why）
/// - 框架只关心“事件被可靠写入”，至于写到哪里由业务定制，例如本地文件、Kafka、对象存储等。
///
/// ## 契约说明（What）
/// - `record`：同步写入事件。若返回错误，上游会视为此次变更失败并触发重试或熔断。
/// - `flush`：可选的刷新钩子，默认返回成功；文件 Recorder 可在此同步落盘。
pub trait AuditRecorder: Send + Sync {
    fn record(&self, event: AuditEventV1) -> Result<(), AuditError>;

    fn flush(&self) -> Result<(), AuditError> {
        Ok(())
    }
}

/// Recorder 返回的错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditError {
    message: Cow<'static, str>,
}

impl AuditError {
    /// 创建带上下文的错误实例。
    pub fn new<M>(message: M) -> Self
    where
        M: Into<Cow<'static, str>>,
    {
        Self {
            message: message.into(),
        }
    }

    /// 返回错误描述。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for AuditError {}

/// 负责生成稳定哈希的辅助器。
///
/// ## 设计动机（Why）
/// - 审计事件要求将变更前后的状态编码为哈希链，以检测事件缺失或篡改。
/// - 通过集中化实现，确保运行时与回放工具之间使用一致的编码逻辑。
pub struct AuditStateHasher;

impl AuditStateHasher {
    /// 计算完整配置映射的哈希。
    ///
    /// ### 参数（Inputs）
    /// - `iter`: 迭代器，按稳定顺序返回 `(ConfigKey, ConfigValue)` 对。
    ///
    /// ### 逻辑（How）
    /// - 对每个键写入字符串形式 `domain::name@scope`；
    /// - 对值执行类型分派，逐项写入标记、原始值或子项；
    /// - 最终返回 SHA-256 的十六进制字符串。
    pub fn hash_configuration<'a, I>(iter: I) -> String
    where
        I: IntoIterator<Item = (&'a ConfigKey, &'a ConfigValue)>,
    {
        let mut hasher = Sha256::new();
        for (key, value) in iter {
            hasher.update(key.to_string().as_bytes());
            hasher.update([0u8]);
            Self::hash_value(&mut hasher, value);
            hasher.update([0xFF]);
        }
        let digest = hasher.finalize();
        hex_encode(&digest)
    }

    fn hash_value(hasher: &mut Sha256, value: &ConfigValue) {
        match value {
            ConfigValue::Boolean(v, meta) => {
                hasher.update(b"bool");
                hasher.update([*v as u8]);
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::Integer(v, meta) => {
                hasher.update(b"int");
                hasher.update(v.to_le_bytes());
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::Float(v, meta) => {
                hasher.update(b"float");
                hasher.update(v.to_bits().to_le_bytes());
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::Text(v, meta) => {
                hasher.update(b"text");
                hasher.update(v.as_bytes());
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::Binary(v, meta) => {
                hasher.update(b"binary");
                hasher.update(v);
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::Duration(duration, meta) => {
                hasher.update(b"duration");
                hasher.update(duration.as_nanos().to_le_bytes());
                Self::hash_metadata(hasher, meta);
            }
            ConfigValue::List(values, meta) => {
                hasher.update(b"list");
                Self::hash_metadata(hasher, meta);
                for item in values {
                    Self::hash_value(hasher, item);
                }
            }
            ConfigValue::Dictionary(entries, meta) => {
                hasher.update(b"dict");
                Self::hash_metadata(hasher, meta);
                let mut sorted = entries.clone();
                sorted.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (key, value) in sorted {
                    hasher.update(key.as_bytes());
                    Self::hash_value(hasher, &value);
                }
            }
        }
    }

    fn hash_metadata(hasher: &mut Sha256, metadata: &crate::configuration::ConfigMetadata) {
        hasher.update([metadata.hot_reloadable as u8]);
        hasher.update([metadata.encrypted as u8]);
        hasher.update([metadata.experimental as u8]);
        for (key, value) in &metadata.tags {
            hasher.update(key.as_bytes());
            hasher.update(value.as_bytes());
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0F) as usize] as char);
    }
    out
}

/// 审计上下文：封装 Recorder 与实体/操作者静态信息。
///
/// ## 使用方式（How）
/// - 构建 [`AuditContext`] 后，通过 `ConfigurationBuilder::with_audit_pipeline` 注入。
/// - 框架在生成事件时会克隆上下文，并补全动态字段（例如实体 ID、哈希、变更集）。
#[derive(Clone)]
pub struct AuditPipeline {
    pub recorder: alloc::sync::Arc<dyn AuditRecorder>,
    pub context: AuditContext,
}

/// 静态审计上下文信息。
///
/// ## 字段说明（What）
/// - `entity_kind`：审计实体类型，例 `configuration.profile`。
/// - `actor`：操作者元信息。
/// - `action`：默认动作描述，可在事件生成时直接使用。
/// - `tsa_evidence`：预先绑定的可信时间证据，可选。
/// - `entity_labels`：补充实体标签（租户、区域等），随事件透传。
#[derive(Clone, Debug)]
pub struct AuditContext {
    pub entity_kind: Cow<'static, str>,
    pub actor: AuditActor,
    pub action: Cow<'static, str>,
    pub tsa_evidence: Option<TsaEvidence>,
    pub entity_labels: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

impl AuditContext {
    /// 创建新的上下文。
    pub fn new(
        entity_kind: Cow<'static, str>,
        actor: AuditActor,
        action: Cow<'static, str>,
    ) -> Self {
        Self {
            entity_kind,
            actor,
            action,
            tsa_evidence: None,
            entity_labels: Vec::new(),
        }
    }

    /// 为事件附加 TSA 证据。
    pub fn with_tsa(mut self, evidence: TsaEvidence) -> Self {
        self.tsa_evidence = Some(evidence);
        self
    }

    /// 设置实体标签集合，便于在多租户或多区域场景中补充上下文。
    pub fn with_entity_labels(
        mut self,
        labels: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    ) -> Self {
        self.entity_labels = labels;
        self
    }
}

#[cfg(feature = "std")]
use alloc::sync::Arc;

#[cfg(feature = "std")]
use std::sync::Mutex;

/// 基于内存的 Recorder，便于测试与演示回放。
#[cfg(feature = "std")]
#[derive(Default)]
pub struct InMemoryAuditRecorder {
    inner: Mutex<InMemoryRecorderState>,
}

#[cfg(feature = "std")]
#[derive(Default)]
struct InMemoryRecorderState {
    events: Vec<AuditEventV1>,
    last_hash: Option<String>,
}

#[cfg(feature = "std")]
impl InMemoryAuditRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回当前已记录的事件副本。
    pub fn events(&self) -> Vec<AuditEventV1> {
        self.inner.lock().expect("poison").events.clone()
    }
}

#[cfg(feature = "std")]
impl AuditRecorder for InMemoryAuditRecorder {
    fn record(&self, event: AuditEventV1) -> Result<(), AuditError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| AuditError::new("recorder poisoned"))?;
        if let Some(previous) = &guard.last_hash
            && previous != &event.state_prev_hash
        {
            return Err(AuditError::new("detected hash gap in audit chain"));
        }
        guard.last_hash = Some(event.state_curr_hash.clone());
        guard.events.push(event);
        Ok(())
    }
}

#[cfg(feature = "std")]
impl InMemoryAuditRecorder {
    /// 构造便于测试使用的 `Arc<dyn AuditRecorder>`。
    pub fn shared(self) -> Arc<dyn AuditRecorder> {
        Arc::new(self)
    }
}
