use crate::{
    BoxFuture,
    buffer::BufferAllocator,
    distributed::{ClusterMembershipProvider, ServiceDiscoveryProvider},
    observability::{HealthCheckProvider, Logger, MetricsProvider, OpsEventBus},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::time::Duration;

/// `Executor` 定义运行时的任务派发接口。
///
/// # 设计背景（Why）
/// - 统一抽象不同平台（Tokio、smol、自研运行时）的任务调度能力。
/// - 允许 `spark-core` 在 `no_std + alloc` 环境中由宿主平台提供执行器实现。
///
/// # 逻辑解析（How）
/// - `spawn` 处理 `Send` 任务；`spawn_blocking` 托管阻塞任务至专用线程池；`spawn_local` 支持 `!Send`。
///
/// # 契约说明（What）
/// - **前置条件**：调用方须确保传入的 Future 符合对应的 `Send` 约束。
/// - **后置条件**：所有任务的生命周期由执行器接管，API 不保证任务完成顺序。
///
/// # 风险提示（Trade-offs）
/// - 未强制要求返回 JoinHandle，以保持接口最小化；如需取消/等待，请在上层实现中扩展。
pub trait Executor: Send + Sync + 'static {
    /// 派发一个 `Send` 任务。
    fn spawn(&self, fut: BoxFuture<'static, ()>);

    /// 将阻塞操作移交给专用线程池。
    fn spawn_blocking(&self, f: Box<dyn FnOnce() + Send + 'static>);

    /// 派发 `!Send` 任务，通常运行在本地线程执行器。
    fn spawn_local(&self, fut: BoxFuture<'static, ()>);
}

/// `Timer` 提供睡眠等时间驱动能力。
///
/// # 设计背景（Why）
/// - 管道中的限速、超时控制依赖统一的计时原语。
///
/// # 逻辑解析（How）
/// - 目前仅暴露 `sleep`，其它接口可由组合实现，保持核心 API 精简。
///
/// # 契约说明（What）
/// - **前置条件**：`duration` 应为非负；实现者可针对 `Duration::ZERO` 做快速返回。
/// - **后置条件**：返回的 `Future` 在计时结束后完成。
pub trait Timer: Send + Sync + 'static {
    /// 异步休眠指定时长。
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

/// `SparkRuntime` 聚合执行器与计时器能力。
///
/// # 设计背景（Why）
/// - 将执行与计时能力统一注入 `CoreServices`，便于 Handler 复用。
///
/// # 契约说明（What）
/// - 实现需同时满足 `Executor + Timer`，并保证 `Send + Sync`。
pub trait SparkRuntime: Executor + Timer + Send + Sync + 'static {}

impl<T> SparkRuntime for T where T: Executor + Timer + Send + Sync + 'static {}

/// `CoreServices` 汇集所有在构建期可注入的能力集合。
///
/// # 设计背景（Why）
/// - 统一依赖注入入口，避免 Handler 直接耦合外部资源。
///
/// # 逻辑解析（How）
/// - 结构体按职责划分：运行时、内存分配、指标日志、集群信息、运维事件、健康探针。
///
/// # 契约说明（What）
/// - **前置条件**：构建者需确保各字段均已初始化，并符合线程安全要求。
/// - **后置条件**：拷贝（`Clone`）后的实例与原值共享底层资源，适合在 Handler 中廉价复制。
///
/// # 风险提示（Trade-offs）
/// - `membership`、`discovery` 可选，允许在单机模式下节省资源；使用前需检查 `Option`。
#[derive(Clone)]
pub struct CoreServices {
    pub runtime: Arc<dyn SparkRuntime>,
    pub allocator: Arc<dyn BufferAllocator>,
    pub metrics: Arc<dyn MetricsProvider>,
    pub logger: Arc<dyn Logger>,
    pub membership: Option<Arc<dyn ClusterMembershipProvider>>,
    pub discovery: Option<Arc<dyn ServiceDiscoveryProvider>>,
    pub ops_bus: Arc<dyn OpsEventBus>,
    pub health_checks: Arc<Vec<Arc<dyn HealthCheckProvider>>>,
}
