//! # Contract-only Runtime Surface
//!
//! ## 契约声明
//! * **Contract-only：** 本模块仅定义运行时可供合约调用的抽象 API，约束业务侧只能依赖这些接口而非具体执行器实现，以确保在无状态执行环境、回放环境中保持一致行为。
//! * **禁止实现：** 本文件及其子模块不允许落地具体执行逻辑，实现必须由宿主运行时或测试替身在独立 crate 中提供，从而杜绝合约代码在此处混入状态机或 I/O 细节。
//! * **解耦外设：** 所有接口均以 `Send + Sync + 'static` 能力描述，对具体执行器、定时器、异步 runtime 完全解耦，便于在 wasm/no-std 等受限环境中替换宿主。
//!
//! ## 并发与错误语义
//! * **并发模型：** 默认遵循单请求上下文内的协作式并发——接口返回的 future/stream 必须可被外部执行器安全地 `await`；禁止在实现中假设特定调度器或线程池。
//! * **错误传播：** 约定使用 `SparkError` 系列枚举表达业务/系统异常；调用方必须准备处理超时、取消、幂等失败等场景，实现方不得吞掉错误或 panic。
//!
//! ## 使用前后条件
//! * **前置条件：** 合约端在调用前必须确保上下文由外部运行时注入（如 tracing span、租户信息），且所有句柄均来自这些契约。
//! * **后置条件：** 成功调用保证返回值可用于继续构造合约逻辑，但不会持有宿主资源的独占所有权，避免破坏运行时调度；任何需要长生命周期资源的对象都必须通过显式托管接口申请。
//!
//! ## 设计取舍提示
//! * **架构选择：** 通过模块化接口换取统一性，牺牲了直接操作底层 executor 的灵活性，却换来可测试性与跨平台部署能力。
//! * **边界情况：** 合约作者需关注超时、重复调度、以及宿主拒绝服务等边界；本模块接口文档会明确每个 API 的退化行为，便于上层实现补偿逻辑。
//!
#[allow(deprecated)]
use crate::observability::LegacyObservabilityHandles;
use crate::{
    buffer::BufferPool,
    cluster::{ClusterMembership, ServiceDiscovery},
    observability::{
        DefaultObservabilityFacade, HealthChecks, Logger, MetricsProvider, ObservabilityFacade,
        OpsEventBus,
    },
};
use alloc::sync::Arc;

use super::{AsyncRuntime, TimeDriver};

/// `CoreServices` 汇集运行时向 Handler 暴露的核心依赖。
///
/// # 设计背景（Why）
/// - 借鉴微服务平台常用的依赖注入容器，将运行时、观测性、分布式能力统一封装。
/// - 结合研究领域关于“最小可用依赖集”（Minimal Viable Dependency Set）的理念，确保在 `no_std`
///   与云原生场景间切换时无需更改上层业务代码。
///
/// # 逻辑解析（How）
/// - `runtime`：聚合任务调度与计时功能，要求实现 [`AsyncRuntime`]。
/// - `buffer_pool`：统一的缓冲租借接口，支持背压控制。
/// - `metrics` / `logger`：可观测性三件套，支撑指标、日志、事件。
/// - `membership` / `discovery`：分布式相关能力，使用 `Option` 以兼容单机场景。
/// - `ops_bus`：运维事件总线，便于广播生命周期事件。
/// - `health_checks`：健康探针集合，供运维面读取。
///
/// # 契约说明（What）
/// - **前置条件**：构建 `CoreServices` 时需确保所有 `Arc` 指向的资源在运行时生命周期内有效。
/// - **后置条件**：克隆 `CoreServices` 不会复制底层资源，仅增加引用计数，适合在 Handler 间传递。
///
/// # 风险提示（Trade-offs）
/// - `membership` 与 `discovery` 在部分部署模式下可能为 `None`；调用前应显式检查并降级处理。
/// - 若 `buffer_pool` 容量不足，建议结合指标与事件总线协同扩容或限流。
#[derive(Clone)]
pub struct CoreServices {
    pub runtime: Arc<dyn AsyncRuntime>,
    pub buffer_pool: Arc<dyn BufferPool>,
    pub metrics: Arc<dyn MetricsProvider>,
    pub logger: Arc<dyn Logger>,
    pub membership: Option<Arc<dyn ClusterMembership>>,
    pub discovery: Option<Arc<dyn ServiceDiscovery>>,
    pub ops_bus: Arc<dyn OpsEventBus>,
    pub health_checks: HealthChecks,
}

impl CoreServices {
    /// 基于 Facade 构造 `CoreServices` 的便捷工厂。
    ///
    /// # 设计动机（Why）
    /// - 统一“运行时 + 缓冲池 + 可观测性”三元组的装配入口，避免手动逐字段填写
    ///   `metrics`/`logger`/`ops_bus`/`health_checks` 造成的样板代码；
    /// - 为 Facade 推广提供正向激励，新代码可直接依赖该工厂，而旧路径仍可通过
    ///   [`LegacyObservabilityHandles`] 适配；
    /// - 在架构层面明确“CoreServices 由 Facade 派生观测依赖”，便于后续演进时统一治理。
    ///
    /// # 行为逻辑（How）
    /// 1. 克隆 Facade 返回的 `Arc` 句柄，填充 `CoreServices` 对应字段；
    /// 2. 将 `membership`/`discovery` 默认设置为 `None`，调用方可在需要时再行赋值；
    /// 3. 健康探针集合通过 `clone` 复制 `Arc`，保持零拷贝开销。
    ///
    /// # 契约说明（What）
    /// - **输入参数**：运行时调度器、缓冲池、实现 [`ObservabilityFacade`] 的 Facade；
    /// - **前置条件**：Facade 内部句柄满足线程安全约束；
    /// - **后置条件**：返回的 `CoreServices` 可直接用于 Pipeline/Router 构造，同时可通过
    ///   [`CoreServices::observability_facade`] 重新获取 Facade。
    ///
    /// # 风险与权衡（Trade-offs）
    /// - 方法会克隆多次 `Arc`（常数成本）；
    /// - 若调用方需要自定义 `membership`/`discovery`，应在返回后手动覆盖；
    /// - 为降低迁移门槛，仍保留 `with_legacy_observability_handles` 兼容层，但建议尽快迁移。
    pub fn with_observability_facade(
        runtime: Arc<dyn AsyncRuntime>,
        buffer_pool: Arc<dyn BufferPool>,
        observability: impl ObservabilityFacade,
    ) -> Self {
        Self {
            runtime,
            buffer_pool,
            metrics: observability.metrics(),
            logger: observability.logger(),
            membership: None,
            discovery: None,
            ops_bus: observability.ops_bus(),
            health_checks: observability.health_checks().clone(),
        }
    }

    /// 旧版“分散句柄”构造器的兼容适配层。
    ///
    /// # 提示
    /// - 保留旧签名以方便存量代码迁移；
    /// - 内部直接复用 [`Self::with_observability_facade`]，确保逻辑单一来源；
    /// - 使用时会触发弃用告警，提醒调用方迁移至 Facade。
    #[allow(deprecated)]
    #[deprecated(
        since = "0.2.0",
        note = "removal: 将在 Facade 全量迁移完毕后移除；migration: 调用 CoreServices::with_observability_facade 并传入 ObservabilityFacade 实现，替代 LegacyObservabilityHandles 构造路径。"
    )]
    pub fn with_legacy_observability_handles(
        runtime: Arc<dyn AsyncRuntime>,
        buffer_pool: Arc<dyn BufferPool>,
        handles: LegacyObservabilityHandles,
    ) -> Self {
        Self::with_observability_facade(runtime, buffer_pool, handles.into_facade())
    }

    /// 提供运行时调度器的便捷访问器，常用于测试中替换实现。
    pub fn runtime(&self) -> &dyn AsyncRuntime {
        self.runtime.as_ref()
    }

    /// 暴露时间驱动能力，便于在无需任务调度时直接访问 [`TimeDriver`]。
    pub fn time_driver(&self) -> &dyn TimeDriver {
        self.runtime.as_ref()
    }

    /// 构造基于当前依赖的可观测性外观。
    ///
    /// # 设计动机（Why）
    /// - 遵循 REQ-SIMP-001 的外观提案，提供统一出口供 Handler 访问可观测性能力，减少字段级别耦合。
    /// - 便于在依赖注入层做能力协商：宿主可在自定义实现中替换返回值以支持多租户、动态开关等高级特性。
    ///
    /// # 使用说明（What）
    /// - **前置条件**：`CoreServices` 内部字段必须已初始化；若某能力不可用应使用显式空实现（如 `NoopLogger`）。
    /// - **后置条件**：返回的外观实现 [`ObservabilityFacade`](crate::observability::ObservabilityFacade)，克隆后仍指向相同的底层 `Arc`，不会额外分配资源。
    ///
    /// # 逻辑解析（How）
    /// - 通过克隆内部 `Arc` 构造 [`DefaultObservabilityFacade`]，确保对象安全且易于在 `no_std + alloc` 环境中使用。
    /// - 该方法每次调用都会复制 `Arc` 的引用计数，如需缓存可在调用方层面持久化返回值。
    ///
    /// # 风险提示（Trade-offs）
    /// - 目前始终返回 `DefaultObservabilityFacade`；如需懒加载或按租户隔离，需要在外层包装自定义结构。
    /// - 若健康探针集合为空，请在调试文档中说明，以免调用方误以为系统缺失探针实现。
    pub fn observability_facade(&self) -> DefaultObservabilityFacade {
        DefaultObservabilityFacade::new(
            Arc::clone(&self.logger),
            Arc::clone(&self.metrics),
            Arc::clone(&self.ops_bus),
            self.health_checks.clone(),
        )
    }
}
