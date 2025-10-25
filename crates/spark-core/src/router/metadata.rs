use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// `MetadataKey` 统一路由属性键名，避免大小写与命名冲突。
///
/// # 设计动机（Why）
/// - 借鉴 Kubernetes Label/Annotation 与 Envoy Metadata 的经验，引入结构化键名，
///   便于跨团队协同定义策略并进行冲突检测。
///
/// # 命名规范（What）
/// - 推荐使用 `segment.segment` 的层级命名方式，如 `traffic.weight`、`observability.trace_id`。
/// - 内部不强制校验，以降低运行期开销；实现者可在上层构建静态分析工具。
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MetadataKey(Cow<'static, str>);

impl MetadataKey {
    /// 新建键名。
    pub fn new(key: Cow<'static, str>) -> Self {
        Self(key)
    }

    /// 返回原始字符串。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `MetadataValue` 封装常见的策略属性值类型。
///
/// # 设计动机（Why）
/// - 参考 AWS App Mesh、Istio Attribute Expression，支持布尔、数值、字符串、列表等形式，
///   便于表达百分比路由、权重、地域等信息。
///
/// # 取舍说明（Trade-offs）
/// - 仅包含最小必要集合，避免在核心契约中引入复杂的序列化依赖；
///   若需扩展可在上层定义自定义解析逻辑或使用二进制编码值。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum MetadataValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(Cow<'static, str>),
    List(Vec<MetadataValue>),
}

/// `RouteMetadata` 是对路由属性的类型安全封装。
///
/// # 设计动机（Why）
/// - 对齐 Envoy Metadata 与 Open Policy Agent 的数据模型，
///   便于进行策略校验、灰度发布、地域路由等高级控制。
/// - `BTreeMap` 能够保持键排序，方便调试与可重复编码，满足差异比对或签名需求。
///
/// # 前置/后置条件
/// - **前置**：键值需由上游组件保证语义正确与大小写一致。
/// - **后置**：迭代结果稳定，可用于哈希/签名或回传管理平面。
///
/// # BTreeMap 性能讨论
/// - 路由元数据写入频率通常远低于读取频率，因此优先选择有序结构以获取确定性输出和稳定序列化顺序。
/// - 与 `HashMap` 相比，`BTreeMap` 在插入、更新时需要 `O(log n)` 对数级旋转；若策略引擎在热路径执行大量写入，可先将 `RouteMetadata`
///   转存为 `HashMap` 或小型缓存结构，加工后再排序回 `BTreeMap` 交付，以换取读多写少场景的稳定性。
/// - 后续若大量场景需要直接返回 `HashMap`，可评估增加 `into_hash_map` 或特性开关；当前版本保持稳定排序以利于审计。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RouteMetadata {
    entries: BTreeMap<MetadataKey, MetadataValue>,
}

impl RouteMetadata {
    /// 创建空白元数据。
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 插入或覆盖一个属性。
    pub fn insert(&mut self, key: MetadataKey, value: MetadataValue) {
        self.entries.insert(key, value);
    }

    /// 读取属性。
    pub fn get(&self, key: &MetadataKey) -> Option<&MetadataValue> {
        self.entries.get(key)
    }

    /// 遍历所有键值对。
    pub fn iter(&self) -> impl Iterator<Item = (&MetadataKey, &MetadataValue)> {
        self.entries.iter()
    }
}
