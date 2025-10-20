use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::time::Duration;

/// 配置项附带的可选元数据。
///
/// ### 设计目标（Why）
/// - 将业界常见的配置属性（可热更新、是否加密、是否实验性）显式化，帮助上层治理策略决策。
/// - 在无 `std` 的情况下仍能表达布尔标记，便于跨平台复用。
///
/// ### 逻辑概览（How）
/// - `hot_reloadable`：是否支持运行时热更新。
/// - `encrypted`：是否需要外部密钥管理器参与解密。
/// - `experimental`：标记当前配置是否尚处稳定性观察期。
/// - `tags`：额外标签，沿用 CNCF 项目推崇的键值标签理念。
///
/// ### 契约说明（What）
/// - **前置条件**：标签键值需满足 UTF-8，可与 `serde`、`prost` 等序列化方案直接映射。
/// - **后置条件**：实现 `Default`，便于调用方仅关注需要的字段。
///
/// ### 设计取舍（Trade-offs）
/// - 仅使用向量存储标签，避免在 `no_std` 场景强行引入 `HashMap`；上层可选择合适的查找结构。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigMetadata {
    pub hot_reloadable: bool,
    pub encrypted: bool,
    pub experimental: bool,
    pub tags: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

/// 配置值的枚举表示。
///
/// ### 设计目标（Why）
/// - 参考 HashiCorp Consul、AWS AppConfig 与 Envoy xDS 对配置值的通用抽象，确保跨生态互操作。
/// - Rust 层使用强类型枚举，避免传统字符串配置带来的解析歧义。
///
/// ### 逻辑解析（How）
/// - 支持基础标量类型（布尔、整数、浮点、字符串、字节、时间间隔）。
/// - 通过 `List` 嵌套表示轻量数组，覆盖多 Endpoint、白名单等场景。
/// - 预留 `Dictionary` 支持键值对结构，但为了无 `std` 约束只允许嵌套 `(key, value)` 列表。
///
/// ### 契约定义（What）
/// - **前置条件**：`Dictionary` 中的键必须唯一；调用方需自行保证。
/// - **输入/输出**：枚举值可配合 [`ConfigMetadata`] 一并返回。
/// - **后置条件**：所有变体均实现 `Clone`，适用于广播、快照与缓存。
///
/// ### 设计取舍与风险（Trade-offs）
/// - 未引入 `serde::Deserialize` 约束，避免强绑序列化方案；实际落地时可在上层实现转换。
/// - `List` 与 `Dictionary` 采用 `Vec` 以保持顺序性，便于与 YAML/JSON 对齐；若需高性能查询可在业务侧转换。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConfigValue {
    Boolean(bool, ConfigMetadata),
    Integer(i64, ConfigMetadata),
    Float(f64, ConfigMetadata),
    Text(Cow<'static, str>, ConfigMetadata),
    Binary(Cow<'static, [u8]>, ConfigMetadata),
    Duration(Duration, ConfigMetadata),
    List(Vec<ConfigValue>, ConfigMetadata),
    Dictionary(Vec<(Cow<'static, str>, ConfigValue)>, ConfigMetadata),
}

impl ConfigValue {
    /// 返回与配置值绑定的元数据。
    ///
    /// ### 契约（What）
    /// - **输入**：对配置值的不可变引用。
    /// - **输出**：元数据的不可变引用。
    ///
    /// ### 逻辑（How）
    /// - 通过匹配枚举变体直接返回引用，不发生克隆。
    #[inline]
    pub fn metadata(&self) -> &ConfigMetadata {
        match self {
            Self::Boolean(_, meta)
            | Self::Integer(_, meta)
            | Self::Float(_, meta)
            | Self::Text(_, meta)
            | Self::Binary(_, meta)
            | Self::Duration(_, meta)
            | Self::List(_, meta)
            | Self::Dictionary(_, meta) => meta,
        }
    }
}
