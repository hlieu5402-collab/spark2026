use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use super::{
    ChangeCallback, ChangeEvent, ChangeNotification, ChangeSet, ConfigKey, ConfigValue,
    ConfigurationError, ConfigurationLayer, ConfigurationSource, ProfileDescriptor, ProfileId,
    ProfileLayering, SourceRegistrationError, WatchToken,
};

/// 已解析的配置结果。
///
/// ### 设计目的（Why）
/// - 将多个配置层按优先顺序合并为最终映射，供运行时快速读取。
/// - 采用 [`BTreeMap`] 保证遍历顺序稳定，便于日志与调试对齐，并为差异化比较提供确定性输出。
///
/// ### 契约定义（What）
/// - `values`：键到值的映射，后注册的高优先级层会覆盖低优先级层。
/// - `version`：递增版本号，用于与通知事件对齐。
///
/// ### BTreeMap 取舍（Trade-offs）
/// - 与 `HashMap` 相比，`BTreeMap` 的写入/合并为 `O(log n)`，在大规模配置层叠加时会稍慢；换来的是确定性序列化便于做快照哈希与审计。
/// - 若调用方在热路径需要更快的随机访问，可将 `values` 克隆为 `HashMap` 进行缓存，或在增量通知中仅同步差异集合以减轻重建成本。
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedConfiguration {
    pub values: BTreeMap<ConfigKey, ConfigValue>,
    pub version: u64,
}

/// 维护分层配置的句柄。
///
/// ### 设计目的（Why）
/// - 提供对最新配置的读取能力，并暴露订阅接口供运行时监听。
/// - 借鉴 Envoy 的 `ConfigSubscription`，将变更管道化。
///
/// ### 契约说明（What）
/// - `profile`：当前句柄绑定的 Profile。
/// - `layers`：存放原始 Layer，保持可追溯性。
/// - `watchers`：记录回调，确保变更时能广播。
///
/// ### 设计取舍（Trade-offs）
/// - 当前实现仅提供同步回调列表；异步扩展可在上层包装。
pub struct LayeredConfiguration {
    profile: ProfileId,
    layering: ProfileLayering,
    layers: Vec<ConfigurationLayer>,
    watchers: Vec<Arc<dyn ChangeCallback + Send + Sync>>, // 简化实现：直接存储回调引用
    version: AtomicU64,
}

impl LayeredConfiguration {
    /// 创建一个新的分层配置实例。
    ///
    /// ### 设计意图（Why）
    /// - Builder 在汇总完初始 Layer 后需要一个承载体保存运行时状态。
    /// - 独立构造函数可让测试或自定义装配流程绕过 `ConfigurationBuilder`。
    ///
    /// ### 契约说明（What）
    /// - **输入**：`profile` 指定当前配置面向的运行环境；`layering` 确定合并策略。
    /// - **前置条件**：调用方需确保 `profile` 的依赖已在更高层完成拓扑排序。
    /// - **后置条件**：返回的实例内部 Layer 列表为空，版本号初始化为 `0`。
    ///
    /// ### 逻辑概要（How）
    /// - 通过简单字段赋值构建结构体，避免隐藏副作用。
    pub fn new(profile: ProfileId, layering: ProfileLayering) -> Self {
        Self {
            profile,
            layering,
            layers: Vec::new(),
            watchers: Vec::new(),
            version: AtomicU64::new(0),
        }
    }

    /// 注册一个配置层。
    ///
    /// ### 设计意图（Why）
    /// - 支持 Builder 或增量同步在运行时追加新的 Layer，例如引入更高优先级的租户级配置。
    ///
    /// ### 契约（What）
    /// - **前置条件**：调用者应按照 `SourceMetadata.priority` 从低到高调用，以保证覆盖顺序正确。
    /// - **后置条件**：Layer 被追加到内部列表末尾，不进行去重。
    ///
    /// ### 注意事项（Trade-offs）
    /// - 若未遵循优先级顺序，后续 `resolve` 仍能工作，但可能产生意料之外的覆盖结果。
    pub fn push_layer(&mut self, layer: ConfigurationLayer) {
        self.layers.push(layer);
    }

    /// 返回当前 Profile。
    #[inline]
    pub fn profile(&self) -> &ProfileId {
        &self.profile
    }

    /// 合并所有 Layer，并返回最新快照。
    ///
    /// ### 逻辑解析（How）
    /// - 根据 `layering` 决定遍历顺序。
    /// - 遍历过程中将键值写入 `BTreeMap`，后写入者覆盖先前值。
    /// - 版本号由增量变更驱动，`resolve` 仅返回当前版本号，避免纯读取导致的虚假变更。
    pub fn resolve(&self) -> ResolvedConfiguration {
        let mut values = BTreeMap::new();
        match self.layering {
            ProfileLayering::BaseFirst => {
                for layer in &self.layers {
                    for (key, value) in &layer.entries {
                        values.insert(key.clone(), value.clone());
                    }
                }
            }
            ProfileLayering::OverrideFirst => {
                for layer in self.layers.iter().rev() {
                    for (key, value) in &layer.entries {
                        values.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        let version = self.version.load(Ordering::SeqCst);
        ResolvedConfiguration { values, version }
    }

    /// 注册配置变更回调。
    ///
    /// ### 设计意图（Why）
    /// - 支持运行时以观察者模式订阅变更，实现配置热更新。
    ///
    /// ### 契约（What）
    /// - **输入**：实现了 [`ChangeCallback`] 的回调指针，通常由上层组件封装。
    /// - **后置条件**：回调被保存，后续调用 [`broadcast`](Self::broadcast) 时会按注册顺序触发。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 当前实现未实现去重，重复注册会导致多次通知；调用方需自行治理。
    pub fn watch(&mut self, callback: Arc<dyn ChangeCallback + Send + Sync>) {
        self.watchers.push(callback);
    }

    /// 向所有观察者广播变更。
    ///
    /// ### 设计意图（Why）
    /// - 当配置发生更新时，确保所有观察者获得一致的增量事件。
    ///
    /// ### 契约（What）
    /// - **输入**：`notification` 为按顺序整理的事件列表。
    /// - **后置条件**：所有回调均被调用；若任一回调返回错误，将提前中止并把错误向上传递。
    ///
    /// ### 性能考量（Trade-offs）
    /// - 采用同步遍历，保证顺序一致性；若回调耗时较长可能阻塞后续回调，使用者可在上层引入异步调度。
    pub fn broadcast(&self, notification: ChangeNotification) -> Result<(), ConfigurationError> {
        for watcher in &self.watchers {
            watcher.on_change(notification.clone())?;
        }
        Ok(())
    }
}

/// Builder 负责组织 Profile 与数据源，生成最终的 [`ConfigurationHandle`]。
#[derive(Default)]
pub struct ConfigurationBuilder {
    profile: Option<ProfileDescriptor>,
    sources: Vec<Box<dyn ConfigurationSource>>, // 采用 trait object 以兼容多种实现
    capacity: Option<usize>,
}

impl ConfigurationBuilder {
    /// 创建新的 Builder。
    ///
    /// ### 设计意图（Why）
    /// - 提供零成本的构造方式，方便调用方按需配置数据源、Profile 等参数。
    /// - 对标业内 Builder 模式（如 Envoy bootstrap、Spring ConfigBuilder），提升可读性。
    ///
    /// ### 契约说明（What）
    /// - 返回的 Builder 内部尚未设置 Profile，也未注册任何配置源。
    /// - 可直接链式调用后续 `with_*` 方法。
    ///
    /// ### 实现细节（How）
    /// - 直接调用 `Default`，确保字段初始化为空集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 限制最多可注册的数据源数量。
    ///
    /// ### 设计意图（Why）
    /// - 借鉴 AWS AppConfig 对“配置源数量”限制，防止误配置导致内存膨胀。
    ///
    /// ### 契约说明（What）
    /// - **输入**：`capacity` 为允许注册的最大源数量。
    /// - **后置条件**：若后续注册超出该数量，将返回 [`SourceRegistrationError::Capacity`]。
    ///
    /// ### 设计取舍（Trade-offs）
    /// - 仅限制数量，不限制来源类型，保持扩展自由度。
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = Some(capacity);
        self
    }

    /// 指定要构建的 Profile。
    ///
    /// ### 设计意图（Why）
    /// - Profile 决定合并策略与继承关系，是装配配置的核心前提。
    ///
    /// ### 契约说明（What）
    /// - **输入**：[`ProfileDescriptor`]，包含 identifier、extends、layering 等信息。
    /// - **后置条件**：Builder 内部保存该 Profile，用于 `build` 阶段解析。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 当前不会校验 `extends` 的循环依赖，建议在生成 Profile 描述时完成验证。
    pub fn with_profile(mut self, descriptor: ProfileDescriptor) -> Self {
        self.profile = Some(descriptor);
        self
    }

    /// 注册新的配置源。
    ///
    /// ### 设计意图（Why）
    /// - 允许调用者按需组合本地文件、集中式配置中心等多种来源，实现分层治理。
    ///
    /// ### 契约说明（What）
    /// - **输入**：实现了 [`ConfigurationSource`] 的对象。
    /// - **前置条件**：若设置了 `capacity`，需保证未超过上限；Builder 会检测重复实例。
    /// - **后置条件**：成功注册后，数据源被保存供 `build` 阶段调用。
    ///
    /// ### 逻辑解析（How）
    /// - 首先检查容量限制，再通过指针比较避免同一实例重复注册。
    /// - 比较策略采用指针相等判断，确保不会误判两个内容相同但实例不同的源。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 指针比较不会检测“逻辑重复”，若两个不同实例访问同一远程配置中心，上层需自行治理。
    pub fn register_source(
        &mut self,
        source: Box<dyn ConfigurationSource>,
    ) -> Result<(), SourceRegistrationError> {
        if self
            .capacity
            .is_some_and(|limit| self.sources.len() >= limit)
        {
            return Err(SourceRegistrationError::Capacity);
        }

        // 这里只进行简单的指针比较，调用方需自行保证不会注册重复实例。
        let new_ptr: *const dyn ConfigurationSource = &*source;
        if self.sources.iter().any(|existing| {
            let existing_ptr: *const dyn ConfigurationSource = &**existing;
            core::ptr::addr_eq(existing_ptr, new_ptr)
        }) {
            return Err(SourceRegistrationError::Duplicate);
        }

        self.sources.push(source);
        Ok(())
    }

    /// 注册 `'static` 生命周期的配置源引用，复用借用型入口。
    ///
    /// ### 设计动机（Why）
    /// - 某些内建数据源以单例形式存在（如内存快照、只读嵌入资源），调用方更倾向于共享引用；
    /// - 通过本方法可避免重复分配 `Box`，并在 API 层显式标注生命周期假设。
    ///
    /// ### 契约说明（What）
    /// - **输入**：`source` 必须在进程生命周期内有效；
    /// - **执行**：内部借助 `super::source::boxed_static_source` 适配为拥有型对象，再复用 [`Self::register_source`] 的去重与容量检查；
    /// - **后置条件**：Builder 仅持有对单例的引用包装，不负责析构。
    pub fn register_source_static(
        &mut self,
        source: &'static (dyn ConfigurationSource),
    ) -> Result<(), SourceRegistrationError> {
        self.register_source(super::source::boxed_static_source(source))
    }

    /// 构建最终的 [`ConfigurationHandle`]。
    ///
    /// ### 设计意图（Why）
    /// - 将多源配置整合为可用的运行时句柄，同时产出初始快照，减少调用方样板代码。
    ///
    /// ### 前置条件（What）
    /// - 必须调用 [`ConfigurationBuilder::with_profile`] 指定 Profile。
    /// - 至少注册一个配置源。
    ///
    /// ### 执行流程（How）
    /// 1. 遍历所有数据源，调用 [`ConfigurationSource::load`] 拉取 Layer。
    /// 2. 依据 `SourceMetadata.priority` 对 Layer 排序，确保优先级高的覆盖低的。
    /// 3. 初始化 [`LayeredConfiguration`] 并写入 Layer，设置初始版本号为 `1`。
    /// 4. 返回配置句柄与首个解析结果，调用方可立即使用。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 若数据源返回的 Layer 在排序前包含重复键，后出现的值将覆盖先前值；这是预期行为。
    pub fn build(self) -> Result<(ConfigurationHandle, ResolvedConfiguration), ConfigurationError> {
        let descriptor = self.profile.ok_or_else(|| {
            ConfigurationError::with_context(
                super::error::ConfigurationErrorKind::Validation,
                "profile is required",
            )
        })?;
        if self.sources.is_empty() {
            return Err(ConfigurationError::with_context(
                super::error::ConfigurationErrorKind::Validation,
                "at least one configuration source is required",
            ));
        }

        let mut layers = Vec::new();
        for source in &self.sources {
            let mut fetched = source.load(&descriptor.identifier)?;
            layers.append(&mut fetched);
        }

        // 按 priority 从低到高排序，确保高优先级覆盖低优先级。
        layers.sort_by_key(|layer| layer.metadata.priority);

        let mut layered =
            LayeredConfiguration::new(descriptor.identifier.clone(), descriptor.layering);
        for layer in layers {
            layered.push_layer(layer);
        }

        layered.version.store(1, Ordering::SeqCst);
        let resolved = layered.resolve();
        let handle = ConfigurationHandle {
            layered,
            descriptor,
            sources: self.sources,
            watch_tokens: Vec::new(),
        };

        Ok((handle, resolved))
    }
}

/// 对外暴露的配置句柄。
///
/// ### 设计目的（Why）
/// - 提供配置读取与变更订阅接口，是运行时访问配置的入口。
/// - 参考 Netflix Archaius、Spring Cloud Config 的 `Environment` 概念。
pub struct ConfigurationHandle {
    pub(crate) layered: LayeredConfiguration,
    pub(crate) descriptor: ProfileDescriptor,
    pub(crate) sources: Vec<Box<dyn ConfigurationSource>>,
    pub(crate) watch_tokens: Vec<Box<dyn WatchToken>>,
}

impl ConfigurationHandle {
    /// 返回 Profile 描述。
    ///
    /// ### 契约说明（What）
    /// - **输出**：构建时传入的 [`ProfileDescriptor`] 引用，包含 layering 等信息。
    /// - 调用方可用于查询当前配置所属环境或渲染文档。
    ///
    /// ### 注意事项（Trade-offs）
    /// - 返回的是不可变引用，确保调用方无法绕过句柄直接修改 Profile。
    #[inline]
    pub fn profile(&self) -> &ProfileDescriptor {
        &self.descriptor
    }

    /// 获取最新配置快照。
    ///
    /// ### 设计意图（Why）
    /// - 允许调用方以幂等方式读取配置，而无需了解内部 Layer 结构。
    ///
    /// ### 契约说明（What）
    /// - **输出**：[`ResolvedConfiguration`]，包含合并后的映射与当前版本号。
    /// - 每次调用均重新执行合并，确保读取到最新状态；若性能敏感，可结合缓存策略使用。
    pub fn snapshot(&self) -> ResolvedConfiguration {
        self.layered.resolve()
    }

    /// 注册配置变更回调。
    ///
    /// ### 设计意图（Why）
    /// - 连接数据源的推送能力与上层组件，打造统一的热更新入口。
    ///
    /// ### 契约说明（What）
    /// - **输入**：实现 [`ChangeCallback`] 的 `Arc` 指针；使用 `Arc` 确保回调可跨线程共享。
    /// - **后置条件**：
    ///   1. 回调被注册进内部观察者列表。
    ///   2. 若数据源实现了 `watch`，其返回的 [`WatchToken`] 会被保存，生命周期与句柄绑定。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 若任何数据源返回 `Err`，当前实现会忽略错误并继续尝试其它源；可在未来扩展为聚合错误。
    pub fn observe(&mut self, callback: Arc<dyn ChangeCallback + Send + Sync>) {
        self.layered.watch(callback.clone());
        // 同步已有来源的 watch 能力
        for source in &self.sources {
            if let Ok(Some(token)) = source.watch(
                &self.descriptor.identifier,
                Box::new(ForwardingCallback {
                    delegate: callback.clone(),
                }),
            ) {
                self.watch_tokens.push(token);
            }
        }
    }
}

/// 负责将数据源回调转发给外部观察者的适配器。
struct ForwardingCallback {
    delegate: Arc<dyn ChangeCallback + Send + Sync>,
}

impl ChangeCallback for ForwardingCallback {
    fn on_change(&self, notification: ChangeNotification) -> Result<(), ConfigurationError> {
        self.delegate.on_change(notification)
    }
}

impl LayeredConfiguration {
    /// 根据最新变更更新内部状态，并返回差异集合。
    ///
    /// ### 使用场景（Why）
    /// - 当数据源推送增量变更时，调用此方法更新快照，并向观察者广播。
    ///
    /// ### 契约说明（What）
    /// - **输入**：[`ChangeNotification`]，包含单调递增的序号与事件列表。
    /// - **前置条件**：通知内的事件需遵循时间顺序；该方法不进行排序。
    /// - **后置条件**：返回的 [`ChangeSet`] 按事件类型分类，同时内部版本号自增并触发观察者回调。
    ///
    /// ### 执行逻辑（How）
    /// 1. 遍历事件，根据类型分别调用 `upsert_entry` 或 `remove_entry`。
    /// 2. 将受影响的键值收集到增量结果中。
    /// 3. 自增版本号并调用 [`broadcast`](Self::broadcast)。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 若通知包含重复事件（同一键多次更新），后一个事件会覆盖前一事件，返回值亦体现最终状态。
    pub fn apply_change(
        &mut self,
        notification: ChangeNotification,
    ) -> Result<ChangeSet, ConfigurationError> {
        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut deleted = Vec::new();

        for event in &notification.events {
            match event {
                ChangeEvent::Created { key, value } => {
                    self.upsert_entry(key.clone(), value.clone(), &mut created);
                }
                ChangeEvent::Updated { key, value } => {
                    self.upsert_entry(key.clone(), value.clone(), &mut updated);
                }
                ChangeEvent::Deleted { key } => {
                    self.remove_entry(key, &mut deleted);
                }
            }
        }

        self.version.fetch_add(1, Ordering::SeqCst);
        self.broadcast(notification.clone())?;

        Ok(ChangeSet {
            created,
            updated,
            deleted,
        })
    }

    /// 在现有 Layer 中插入或更新配置项。
    ///
    /// ### 契约说明（What）
    /// - **输入**：目标 `key`、`value` 以及用于收集结果的 `bucket`。
    /// - **后置条件**：若找到同名键则覆盖，否则追加到最高优先级 Layer。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 若内部不存在任何 Layer，该操作将被忽略；调用方需保证至少存在一个 Layer。
    fn upsert_entry(
        &mut self,
        key: ConfigKey,
        value: ConfigValue,
        bucket: &mut Vec<(ConfigKey, ConfigValue)>,
    ) {
        for layer in &mut self.layers {
            // 简化策略：寻找第一个包含该键的层进行覆盖，否则插入优先级最高的层。
            if let Some(existing) = layer
                .entries
                .iter_mut()
                .find(|(existing_key, _)| existing_key == &key)
            {
                *existing = (key.clone(), value.clone());
                bucket.push((key, value));
                return;
            }
        }

        if let Some(last_layer) = self.layers.last_mut() {
            last_layer.entries.push((key.clone(), value.clone()));
            bucket.push((key, value));
        }
    }

    /// 从 Layer 中删除配置项。
    ///
    /// ### 契约说明（What）
    /// - **输入**：待删除的 `key` 与结果集合 `bucket`。
    /// - **后置条件**：若找到对应项则移除，并将键记录到删除列表；未找到时静默忽略。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 删除操作仅影响首次匹配的 Layer；如存在多层重复键，剩余层仍保留，保证覆盖语义。
    fn remove_entry(&mut self, key: &ConfigKey, bucket: &mut Vec<ConfigKey>) {
        for layer in &mut self.layers {
            if let Some(index) = layer
                .entries
                .iter()
                .position(|(existing_key, _)| existing_key == key)
            {
                layer.entries.remove(index);
                bucket.push(key.clone());
                return;
            }
        }
    }
}
