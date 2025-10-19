use crate::{
    buffer::BufferPool,
    cluster::{ClusterMembership, ServiceDiscovery},
    observability::{
        DefaultObservabilityFacade, HealthChecks, Logger, MetricsProvider, OpsEventBus,
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
