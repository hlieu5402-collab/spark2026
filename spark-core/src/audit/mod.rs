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

use crate::configuration::{ChangeSet, ConfigKey, ConfigKeyRepr, ConfigValue, ConfigValueRepr};

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
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEntityRef {
    pub kind: Cow<'static, str>,
    pub id: String,
    pub labels: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

impl AuditEntityRef {
    /// ## 设计动机（Why）
    /// - 通过内部表示将公共 API 与具体序列化框架解耦，避免对 `serde` 的硬依赖。
    /// - 便于未来根据不同媒介（JSON、MessagePack 等）灵活生成载荷。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`，要求调用者已构造出合法的实体引用。
    /// - 返回：[`AuditEntityRefRepr`]，仅供当前模块序列化使用。
    ///
    /// ## 逻辑解析（How）
    /// - 对全部字段执行浅克隆，使内部表示拥有独立所有权，便于跨线程与生命周期传递。
    /// - `labels` 直接克隆向量，确保键值在序列化过程中保持稳定顺序。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：上层需保证字段内容满足业务命名规范。
    /// - 后置：返回结构不会修改原实例，可安全重复调用。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 牺牲一次拷贝换取类型隔离；审计实体字段规模有限，该成本可忽略。
    pub(crate) fn to_repr(&self) -> AuditEntityRefRepr {
        AuditEntityRefRepr {
            kind: self.kind.clone(),
            id: self.id.clone(),
            labels: self.labels.clone(),
        }
    }

    /// ## 设计动机（Why）
    /// - 从序列化层的中间表示恢复领域模型，满足反序列化和回放需求。
    ///
    /// ## 契约定义（What）
    /// - 入参：`repr`，经 `serde` 语法校验后的内部表示。
    /// - 返回：[`AuditEntityRef`]，供业务层继续使用。
    ///
    /// ## 逻辑解析（How）
    /// - 逐字段移动内部表示的所有权，避免多余分配。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：尚未进行业务层的合法性校验，需要调用方在之后补充。
    /// - 后置：保证 `labels` 始终返回空向量而非 `None`，维持既有契约。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 保持 `pub(crate)` 可见性，避免外部绕过类型抽象直接依赖内部表示。
    pub(crate) fn from_repr(repr: AuditEntityRefRepr) -> Self {
        Self {
            kind: repr.kind,
            id: repr.id,
            labels: repr.labels,
        }
    }
}

impl serde::Serialize for AuditEntityRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditEntityRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditEntityRefRepr::deserialize(deserializer)?;
        Ok(Self::from_repr(repr))
    }
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
#[derive(Clone, Debug, PartialEq)]
pub struct AuditActor {
    pub id: Cow<'static, str>,
    pub display_name: Option<Cow<'static, str>>,
    pub tenant: Option<Cow<'static, str>>,
}

impl AuditActor {
    /// ## 设计动机（Why）
    /// - 与其他审计结构保持一致，通过内部表示隐藏对 `serde` 的直接依赖。
    /// - 支持未来在不破坏类型定义的情况下扩展额外字段。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`AuditActorRepr`]，仅供序列化通道使用。
    ///
    /// ## 逻辑解析（How）
    /// - 克隆 `Cow` 字段，确保内部表示拥有独立生命周期。
    /// - 保持 `Option` 语义不变，避免出现空字符串等歧义值。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：调用者需保证 `id` 为非空、稳定标识。
    /// - 后置：返回值与原结构字段一一对应，可安全传递给 `serde`。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 额外拷贝换取接口稳定性；相比审计事件写入频率，该成本可接受。
    pub(crate) fn to_repr(&self) -> AuditActorRepr {
        AuditActorRepr {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            tenant: self.tenant.clone(),
        }
    }

    /// ## 设计动机（Why）
    /// - 从中间表示恢复操作者信息，支撑 CLI 与后端共享数据格式。
    ///
    /// ## 契约定义（What）
    /// - 入参：`repr`，内部序列化表示。
    /// - 返回：[`AuditActor`]。
    ///
    /// ## 逻辑解析（How）
    /// - 将内部表示的所有权直接转移至领域模型，避免重复分配。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：字段已通过语法校验，但业务校验仍由上层负责。
    /// - 后置：返回结构中的可选字段若缺省则保持 `None`。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 函数仅在 crate 内可见，防止外部绕过模型接口。
    pub(crate) fn from_repr(repr: AuditActorRepr) -> Self {
        Self {
            id: repr.id,
            display_name: repr.display_name,
            tenant: repr.tenant,
        }
    }
}

impl serde::Serialize for AuditActor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditActor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditActorRepr::deserialize(deserializer)?;
        Ok(Self::from_repr(repr))
    }
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
#[derive(Clone, Debug, PartialEq)]
pub struct TsaEvidence {
    pub provider: Cow<'static, str>,
    pub evidence: String,
    pub issued_at: u64,
}

impl TsaEvidence {
    /// ## 设计动机（Why）
    /// - 通过内部表示隐藏序列化细节，保持公共 API 的纯净度。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`TsaEvidenceRepr`]`，可直接交由 `serde` 处理。
    ///
    /// ## 逻辑解析（How）
    /// - 克隆 `provider` 与 `evidence`，复制 `issued_at`，形成独立的中间表示。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：调用方保证字段值已满足业务要求（例如证据格式、时间范围）。
    /// - 后置：返回表示不会反向影响原结构，适合在多线程中复用。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 为了保证类型隔离选择深拷贝，但对象尺寸较小，性能影响可忽略。
    pub(crate) fn to_repr(&self) -> TsaEvidenceRepr {
        TsaEvidenceRepr {
            provider: self.provider.clone(),
            evidence: self.evidence.clone(),
            issued_at: self.issued_at,
        }
    }

    /// ## 设计动机（Why）
    /// - 从序列化表示还原 TSA 锚点，支撑审计事件在不同系统间传递。
    ///
    /// ## 契约定义（What）
    /// - 入参：`repr`。
    /// - 返回：[`TsaEvidence`]`。
    ///
    /// ## 逻辑解析（How）
    /// - 将内部表示的字段直接移动到新结构，避免重复分配。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：反序列化阶段已确保字段类型正确。
    /// - 后置：返回实例未对证据有效性做进一步校验。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 将签名校验留给业务层，保持模块职责单一。
    pub(crate) fn from_repr(repr: TsaEvidenceRepr) -> Self {
        Self {
            provider: repr.provider,
            evidence: repr.evidence,
            issued_at: repr.issued_at,
        }
    }
}

impl serde::Serialize for TsaEvidence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for TsaEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = TsaEvidenceRepr::deserialize(deserializer)?;
        Ok(Self::from_repr(repr))
    }
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
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEventV1 {
    pub event_id: String,
    pub sequence: u64,
    pub entity: AuditEntityRef,
    pub action: Cow<'static, str>,
    pub state_prev_hash: String,
    pub state_curr_hash: String,
    pub actor: AuditActor,
    pub occurred_at: u64,
    pub tsa_evidence: Option<TsaEvidence>,
    pub changes: AuditChangeSet,
}

impl AuditEventV1 {
    /// ## 设计动机（Why）
    /// - 通过内部表示统一控制序列化行为，确保公共 API 不暴露 `serde` 类型。
    /// - 为未来多格式输出（JSON、YAML、二进制协议）预留扩展空间。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`AuditEventV1Repr`]，供序列化栈使用。
    ///
    /// ## 逻辑解析（How）
    /// - 逐字段克隆标量值与 `Cow` 字符串，调用子结构的 `to_repr` 获取嵌套表示。
    /// - `tsa_evidence` 通过 `Option::map` 转换，保持可选语义。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：事件已经过业务层校验（哈希、序列、权限等）。
    /// - 后置：返回表示拥有完整所有权，可安全传递到任意序列化上下文。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 复制字段带来的额外成本换来 API 稳定性与后续扩展能力。
    pub(crate) fn to_repr(&self) -> AuditEventV1Repr {
        AuditEventV1Repr {
            event_id: self.event_id.clone(),
            sequence: self.sequence,
            entity: self.entity.to_repr(),
            action: self.action.clone(),
            state_prev_hash: self.state_prev_hash.clone(),
            state_curr_hash: self.state_curr_hash.clone(),
            actor: self.actor.to_repr(),
            occurred_at: self.occurred_at,
            tsa_evidence: self.tsa_evidence.as_ref().map(TsaEvidence::to_repr),
            changes: self.changes.to_repr(),
        }
    }

    /// ## 设计动机（Why）
    /// - 集中管理反序列化逻辑，便于未来 Schema 升级时处理兼容策略。
    ///
    /// ## 契约定义（What）
    /// - 入参：`repr`，内部序列化表示。
    /// - 返回：`Result<AuditEventV1, String>`，失败时携带可读错误信息。
    ///
    /// ## 逻辑解析（How）
    /// - 逐字段调用子结构的 `from_repr`，恢复原始领域模型。
    /// - 对 `Option` 字段进行 `map` 转换，保留缺省语义。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：`repr` 已通过 `serde` 的结构校验。
    /// - 后置：返回实例与原输入语义等价，可直接用于回放或校验。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 通过集中转换点可以在未来 Schema 演进时加入默认值或迁移逻辑，降低破坏性改动。
    pub(crate) fn from_repr(repr: AuditEventV1Repr) -> Result<Self, String> {
        let changes = AuditChangeSet::from_repr(repr.changes)
            .map_err(|err| format!("invalid change set: {}", err))?;
        Ok(Self {
            event_id: repr.event_id,
            sequence: repr.sequence,
            entity: AuditEntityRef::from_repr(repr.entity),
            action: repr.action,
            state_prev_hash: repr.state_prev_hash,
            state_curr_hash: repr.state_curr_hash,
            actor: AuditActor::from_repr(repr.actor),
            occurred_at: repr.occurred_at,
            tsa_evidence: repr.tsa_evidence.map(TsaEvidence::from_repr),
            changes,
        })
    }
}

impl serde::Serialize for AuditEventV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditEventV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditEventV1Repr::deserialize(deserializer)?;
        Self::from_repr(repr).map_err(serde::de::Error::custom)
    }
}

/// 审计事件中用于回放的差异集合。
///
/// ## 设计动机（Why）
/// - 复用配置模块已有的 `ChangeSet` 语义，同时将键和值转换为可序列化结构。
/// - 在保持最小字段的前提下，确保 CLI 工具能够完全复现非脱敏的样例数据。
#[derive(Clone, Debug, PartialEq)]
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

    /// ## 设计动机（Why）
    /// - 将差异集合转换为内部表示，从而隐藏 `serde` 依赖并统一序列化入口。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`AuditChangeSetRepr`]，供序列化使用。
    ///
    /// ## 逻辑解析（How）
    /// - 对三类变更分别迭代调用条目级 `to_repr`，确保结构一致。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：调用方应确保 `created`、`updated`、`deleted` 的键互斥。
    /// - 后置：返回表示拥有独立所有权，可在序列化过程中长期存活。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 进行深拷贝以换取生命周期简化；考虑到变更数量通常有限，该开销可接受。
    pub(crate) fn to_repr(&self) -> AuditChangeSetRepr {
        AuditChangeSetRepr {
            created: self.created.iter().map(AuditChangeEntry::to_repr).collect(),
            updated: self.updated.iter().map(AuditChangeEntry::to_repr).collect(),
            deleted: self
                .deleted
                .iter()
                .map(AuditDeletedEntry::to_repr)
                .collect(),
        }
    }

    /// ## 设计动机（Why）
    /// - 支持从中间表示恢复差异集合，保证回放流程的对等性。
    ///
    /// ## 契约定义（What）
    /// - 入参：`repr`。
    /// - 返回：`Result<AuditChangeSet, String>`，失败时包含详细错误信息。
    ///
    /// ## 逻辑解析（How）
    /// - 对内部表示的向量执行 `into_iter` 并调用条目级 `from_repr` 还原。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：`repr` 已通过 `serde` 成功解析。
    /// - 后置：返回结构满足原始语义，可直接传递给回放与校验逻辑。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 保持函数 `pub(crate)`，防止外部组件绕过类型抽象直接依赖内部表示。
    pub(crate) fn from_repr(repr: AuditChangeSetRepr) -> Result<Self, String> {
        let created = repr
            .created
            .into_iter()
            .enumerate()
            .map(|(idx, entry)| {
                AuditChangeEntry::from_repr(entry).map_err(|err| {
                    format!("invalid created change entry at index {}: {}", idx, err)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let updated = repr
            .updated
            .into_iter()
            .enumerate()
            .map(|(idx, entry)| {
                AuditChangeEntry::from_repr(entry).map_err(|err| {
                    format!("invalid updated change entry at index {}: {}", idx, err)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let deleted = repr
            .deleted
            .into_iter()
            .enumerate()
            .map(|(idx, entry)| {
                AuditDeletedEntry::from_repr(entry).map_err(|err| {
                    format!("invalid deleted change entry at index {}: {}", idx, err)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            created,
            updated,
            deleted,
        })
    }
}

impl serde::Serialize for AuditChangeSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditChangeSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditChangeSetRepr::deserialize(deserializer)?;
        Self::from_repr(repr).map_err(serde::de::Error::custom)
    }
}

/// 表示单个新增或更新的配置条目。
///
/// ## 设计动机（补充）
/// - 序列化时会调用 `ConfigKey::to_repr` 与 `ConfigValue::to_repr`，在不暴露 `serde` 依赖的情况下完成 JSON 编码。
/// - 反序列化阶段若检测到非法作用域或不合法的时间间隔，会转换为 `serde` 的 `custom` 错误，便于上层诊断输入问题。
#[derive(Clone, Debug, PartialEq)]
pub struct AuditChangeEntry {
    pub key: ConfigKey,
    pub value: ConfigValue,
}

impl serde::Serialize for AuditChangeEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditChangeEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditChangeEntryRepr::deserialize(deserializer)?;
        AuditChangeEntry::from_repr(repr).map_err(serde::de::Error::custom)
    }
}

impl AuditChangeEntry {
    /// ## 设计动机（Why）
    /// - 提供统一的内部表示转换入口，隐藏 `serde` 依赖的同时维持审计条目的可序列化能力。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`AuditChangeEntryRepr`]，包含可序列化的键和值。
    ///
    /// ## 逻辑解析（How）
    /// - 调用配置模块提供的 `to_repr` 方法，生成拥有所有权的键和值表示。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：当前结构需已通过业务校验。
    /// - 后置：返回的内部表示不会影响原实例，可安全地交由 `serde` 序列化。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 深拷贝不可避免，但换来类型隔离与生命周期简化，适用于审计变更规模有限的场景。
    pub(crate) fn to_repr(&self) -> AuditChangeEntryRepr {
        AuditChangeEntryRepr {
            key: self.key.to_repr(),
            value: self.value.to_repr(),
        }
    }

    /// ## 设计动机（Why）
    /// - 从内部表示恢复领域模型，支撑 CLI、回放器及服务端之间的数据互通。
    ///
    /// ## 契约定义（What）
    /// - 入参：[`AuditChangeEntryRepr`]，通常由 `serde` 反序列化获得。
    /// - 返回：成功时为 [`AuditChangeEntry`]，失败时返回描述性错误字符串。
    ///
    /// ## 逻辑解析（How）
    /// - 分别调用 `ConfigKey::from_repr` 与 `ConfigValue::from_repr`，并为错误附加上下文信息。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：输入在语法上合法，但仍可能违反业务约束。
    /// - 后置：成功则构造出完整条目；失败则不产生任何副作用。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 选择 `String` 作为错误类型，方便直接映射到 `serde` 的 `custom` 错误提示。
    pub(crate) fn from_repr(repr: AuditChangeEntryRepr) -> Result<Self, String> {
        let key = ConfigKey::from_repr(repr.key)
            .map_err(|err| format!("invalid config key in change entry: {}", err))?;
        let value = ConfigValue::from_repr(repr.value)
            .map_err(|err| format!("invalid config value in change entry: {}", err))?;
        Ok(Self { key, value })
    }
}

/// 表示单个删除的配置条目。
///
/// ## 设计提醒（Trade-offs）
/// - 与新增/更新逻辑一致，通过内部表示完成序列化；非法作用域同样会映射为反序列化错误。
#[derive(Clone, Debug, PartialEq)]
pub struct AuditDeletedEntry {
    pub key: ConfigKey,
}

impl serde::Serialize for AuditDeletedEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_repr().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for AuditDeletedEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let repr = AuditDeletedEntryRepr::deserialize(deserializer)?;
        AuditDeletedEntry::from_repr(repr).map_err(serde::de::Error::custom)
    }
}

impl AuditDeletedEntry {
    /// ## 设计动机（Why）
    /// - 与新增/更新条目保持一致的内部表示转换，统一序列化策略。
    ///
    /// ## 契约定义（What）
    /// - 入参：`&self`。
    /// - 返回：[`AuditDeletedEntryRepr`]。
    ///
    /// ## 逻辑解析（How）
    /// - 调用配置模块的 `to_repr`，生成拥有所有权的键信息。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：键已通过业务层校验。
    /// - 后置：返回结构不共享可变状态，可直接交由 `serde` 序列化。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 通过深拷贝换取生命周期简单性；考虑到删除条目数量有限，此开销可控。
    pub(crate) fn to_repr(&self) -> AuditDeletedEntryRepr {
        AuditDeletedEntryRepr {
            key: self.key.to_repr(),
        }
    }

    /// ## 设计动机（Why）
    /// - 支持从序列化表示恢复删除条目，确保审计回放能完整还原差异。
    ///
    /// ## 契约定义（What）
    /// - 入参：[`AuditDeletedEntryRepr`]。
    /// - 返回：成功时为 [`AuditDeletedEntry`]，失败时为描述性错误字符串。
    ///
    /// ## 逻辑解析（How）
    /// - 调用 `ConfigKey::from_repr`，并在错误消息中附加上下文便于诊断非法作用域等问题。
    ///
    /// ## 前置/后置条件（Contract）
    /// - 前置：输入在语法上合法。
    /// - 后置：成功时即可执行删除逻辑；失败时不会影响系统状态。
    ///
    /// ## 设计考量（Trade-offs）
    /// - 返回 `String` 以便与 `serde` 的 `custom` 错误无缝衔接。
    pub(crate) fn from_repr(repr: AuditDeletedEntryRepr) -> Result<Self, String> {
        let key = ConfigKey::from_repr(repr.key)
            .map_err(|err| format!("invalid config key in deleted entry: {}", err))?;
        Ok(Self { key })
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditChangeEntryRepr {
    key: ConfigKeyRepr,
    value: ConfigValueRepr,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditDeletedEntryRepr {
    key: ConfigKeyRepr,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditEntityRefRepr {
    kind: Cow<'static, str>,
    id: String,
    #[serde(default)]
    labels: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditActorRepr {
    id: Cow<'static, str>,
    #[serde(default)]
    display_name: Option<Cow<'static, str>>,
    #[serde(default)]
    tenant: Option<Cow<'static, str>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TsaEvidenceRepr {
    provider: Cow<'static, str>,
    evidence: String,
    issued_at: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditChangeSetRepr {
    created: Vec<AuditChangeEntryRepr>,
    updated: Vec<AuditChangeEntryRepr>,
    deleted: Vec<AuditDeletedEntryRepr>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct AuditEventV1Repr {
    event_id: String,
    sequence: u64,
    entity: AuditEntityRefRepr,
    action: Cow<'static, str>,
    state_prev_hash: String,
    state_curr_hash: String,
    actor: AuditActorRepr,
    occurred_at: u64,
    #[serde(default)]
    tsa_evidence: Option<TsaEvidenceRepr>,
    changes: AuditChangeSetRepr,
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

//
// 教案级说明：`loom` 运行时需要接管互斥锁以枚举调度交错，因此在模型检查配置下
// 显式切换到 `loom::sync::Mutex`。在常规构建中仍保留 `std::sync::Mutex`，确保运行时
// 行为与既有实现一致且无额外性能损耗。
#[cfg(all(feature = "std", any(loom, spark_loom)))]
use loom::sync::Mutex;
#[cfg(all(feature = "std", not(any(loom, spark_loom))))]
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
