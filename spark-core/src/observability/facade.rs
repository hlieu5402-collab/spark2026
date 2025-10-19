//! 可观测性外观层 (Facade) 原型契约。
//!
//! # 设计缘起（Why）
//! - 面向 REQ-SIMP-001 中 T4 建议，解决调用方在注入日志、指标、运维事件时需同时管理多个 `Arc` 的样板问题。
//! - 统一管理可观测性能力，便于在运行时（如 [`CoreServices`](crate::runtime::CoreServices)）中进行能力协商与按需降级。
//! - 参考 AWS Observability Access Points、OpenTelemetry Service Provider Interface 等成熟外观设计，确保后续可以平滑扩展 Trace、审计等能力。
//!
//! # 总体结构（How）
//! - [`ObservabilityFacade`] Trait 定义日志、指标、运维事件与健康探针的最小访问集，强调对象安全与 `no_std + alloc` 兼容性。
//! - [`DefaultObservabilityFacade`] 以克隆 `Arc` 的方式封装现有实现，作为最小可行 (MVP) 实例，后续可替换为宿主自定义聚合结构。
//! - 该模块仅关注契约层设计，不绑定具体实现细节，方便集成方结合自身观测体系自定义。
//!
//! # 契约约束（What）
//! - **前置条件**：实现者需保证返回的 `Arc` 持续有效，且满足对应 Trait (`Logger`、`MetricsProvider`、`OpsEventBus`) 的线程安全约束。
//! - **后置条件**：通过外观获取的能力可在 Handler 生命周期内稳定复用；若某项能力不可用，应通过返回的具体实现自行处理降级。
//! - **输入/输出**：Trait 方法不接受外部参数，直接返回聚合后的句柄引用或克隆，减少调用复杂度。
//!
//! # 风险与权衡（Trade-offs）
//! - 当前返回 `Arc` 克隆，可能带来轻微引用计数开销；若在极端高频路径，可用自定义实现缓存句柄以消除多余克隆。
//! - 外观暂未直接包含 Trace；这是为了避免在初始阶段过度耦合，未来可以通过扩展方法或组合特性引入。
//! - 为保证兼容性，本模块不修改现有 Trait，采用“增量外观”策略；既可原地使用旧字段，也可逐步迁移到 Facade。

use crate::observability::{HealthChecks, Logger, MetricsProvider, OpsEventBus};
use alloc::sync::Arc;

/// 可观测性能力的统一访问接口。
///
/// # 设计目标（Why）
/// - **集中注入点**：为运行时、路由或 Handler 提供单一入口，避免在构造函数中传入多个可观测性句柄。
/// - **一致性保障**：确保日志、指标与运维事件的语义一致，便于跨语言、跨运行时比较与协作。
/// - **演进基础**：为后续的配置化可观测性策略（如动态指标开关、Ops 事件重定向）留出扩展点。
///
/// # 合约说明（What）
/// - `logger`/`metrics`/`ops_bus`：返回对应能力的 `Arc` 克隆，调用方无需关心底层实现。
/// - `health_checks`：返回共享健康探针集合的只读引用，便于对齐平台健康检查。
/// - **前置条件**：实现必须线程安全（`Send + Sync + 'static`），并确保返回的资源生命周期不短于 Facade 本身。
/// - **后置条件**：调用方经由 Facade 获取的句柄在整个生命周期内保持语义一致；若实现提供了能力探针，应自行处理不可用场景。
///
/// # 逻辑解析（How）
/// - Trait 使用对象安全签名，允许以 `Arc<dyn ObservabilityFacade>` 形式注入。
/// - `health_checks` 返回引用而非克隆，避免在高频读取中重复复制向量结构；如需持久化可由调用方显式克隆。
///
/// # 风险提示（Trade-offs）
/// - 统一入口可能隐藏某些实现特有的扩展方法；对于高度定制化需求，可通过具体类型暴露额外 API。
/// - 若宿主未启用健康探针，可返回空集合，但应在实现文档中显式说明以免误用。
pub trait ObservabilityFacade: Send + Sync + 'static {
    /// 获取结构化日志能力。
    fn logger(&self) -> Arc<dyn Logger>;

    /// 获取指标采集能力。
    fn metrics(&self) -> Arc<dyn MetricsProvider>;

    /// 获取运维事件总线能力。
    fn ops_bus(&self) -> Arc<dyn OpsEventBus>;

    /// 访问健康检查集合的共享引用。
    fn health_checks(&self) -> &HealthChecks;
}

/// 以现有 `Arc` 组合实现的参考外观。
///
/// # 设计背景（Why）
/// - 服务于“原型验证”阶段：证明在不破坏既有字段的情况下，可通过外观整合可观测性能力。
/// - 提供默认实现以便测试与示例使用，降低迁移门槛。
///
/// # 构成说明（What）
/// - 持有日志、指标、运维事件的 `Arc` 克隆，以及共享的健康探针集合。
/// - **前置条件**：构造时传入的所有句柄必须满足各自 Trait 的线程安全约束。
/// - **后置条件**：外观本身可 `Clone`，克隆后仍指向同一底层资源，确保 Handler 间安全传递。
///
/// # 逻辑解析（How）
/// - `new` 构造函数直接存储来访句柄，不做额外校验；如需校验可在宿主层扩展。
/// - `ObservabilityFacade` 实现简单转发对应字段，保证零额外逻辑成本。
///
/// # 风险提示（Trade-offs）
/// - 该结构作为教学示例未实现能力探针或懒加载，如需复杂策略请自定义实现。
/// - `health_checks` 直接共享引用，如需写时复制需另行封装。
#[derive(Clone)]
pub struct DefaultObservabilityFacade {
    logger: Arc<dyn Logger>,
    metrics: Arc<dyn MetricsProvider>,
    ops_bus: Arc<dyn OpsEventBus>,
    health_checks: HealthChecks,
}

impl DefaultObservabilityFacade {
    /// 构造组合后的外观实例。
    ///
    /// # 参数（Inputs）
    /// - `logger`: 结构化日志句柄，需实现 [`Logger`].
    /// - `metrics`: 指标采集提供者，需实现 [`MetricsProvider`].
    /// - `ops_bus`: 运维事件总线，需实现 [`OpsEventBus`].
    /// - `health_checks`: 共享健康探针集合，一般来自运行时依赖注入。
    ///
    /// # 契约说明（What）
    /// - **前置条件**：所有参数均不可为 `None`；如无对应能力请在外层使用空实现（如 `NoopLogger`）。
    /// - **后置条件**：返回的外观可在多线程环境下安全克隆与共享，内部资源引用计数保持一致。
    ///
    /// # 设计考量（Trade-offs）
    /// - 采用 `Arc` 而非裸引用以确保对象安全；若宿主希望延迟初始化，可包装在 `OnceLock` 后再传入。
    pub fn new(
        logger: Arc<dyn Logger>,
        metrics: Arc<dyn MetricsProvider>,
        ops_bus: Arc<dyn OpsEventBus>,
        health_checks: HealthChecks,
    ) -> Self {
        Self {
            logger,
            metrics,
            ops_bus,
            health_checks,
        }
    }
}

impl ObservabilityFacade for DefaultObservabilityFacade {
    fn logger(&self) -> Arc<dyn Logger> {
        Arc::clone(&self.logger)
    }

    fn metrics(&self) -> Arc<dyn MetricsProvider> {
        Arc::clone(&self.metrics)
    }

    fn ops_bus(&self) -> Arc<dyn OpsEventBus> {
        Arc::clone(&self.ops_bus)
    }

    fn health_checks(&self) -> &HealthChecks {
        &self.health_checks
    }
}
