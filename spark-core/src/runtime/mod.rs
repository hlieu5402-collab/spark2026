//! 运行时能力聚合模块。
//!
//! # 设计综述（Why）
//! - 以跨平台通信内核为目标，将任务调度、计时驱动、基础服务注入抽象解耦。
//! - 对标 Tokio、Actix 等成熟运行时的稳定设计，结合学术界关于可观测调度的最新研究结果。
//!
//! # 模块结构（How）
//! - `task`：定义任务的生命周期语义与句柄契约。
//! - `executor`：封装运行时代办的任务提交流程与优先级调度参数。
//! - `timer`：统一时间原语，支持单调时间点与延迟等待。
//! - `services`：声明运行时向上层暴露的依赖注入集合。
//!
//! # 使用契约（What）
//! - 宿主需实现 [`TaskExecutor`] 与 [`TimeDriver`]，并通过 [`AsyncRuntime`] 聚合。
//! - 运行时所有对象须满足 `Send + Sync + 'static`，以支撑多线程与 `no_std + alloc` 环境。
//!
//! # 风险提示（Trade-offs）
//! - 聚合接口强调上下文传播，执行器实现必须在 `spawn` 中处理 [`CallContext`](crate::contract::CallContext)；
//!   若运行时直接委托第三方执行器（如 Tokio），需自行封装一层以保证取消/截止信号不会丢失。
//! - 若使用 `spawn_local` 运行 `!Send` 任务，请确保宿主线程生命周期覆盖任务执行窗口（该能力需运行时自行扩展）。

mod executor;
mod services;
mod slo;
mod task;
mod timeouts;
mod timer;

pub use executor::TaskExecutor;
pub use services::CoreServices;
pub use slo::{
    SloPolicyAction, SloPolicyConfigError, SloPolicyDirective, SloPolicyManager,
    SloPolicyReloadReport, SloPolicyRule, SloPolicyTrigger, slo_policy_table_key,
};
pub use task::{
    BlockingTaskSubmission, JoinHandle, LocalTaskSubmission, ManagedBlockingTask, ManagedLocalTask,
    ManagedSendTask, SendTaskSubmission, TaskCancellationStrategy, TaskError, TaskHandle,
    TaskLaunchOptions, TaskPriority, TaskResult,
};
pub use timeouts::{TimeoutConfigError, TimeoutRuntimeConfig, TimeoutSettings};
pub use timer::{MonotonicTimePoint, TimeDriver};

/// `AsyncRuntime` 聚合任务调度与时间驱动能力。
///
/// # 设计背景（Why）
/// - 维持运行时最小组合接口，既可对接成熟执行器（Tokio/async-std），亦方便在嵌入式场景自定义实现。
/// - 聚焦跨平台通信需求，保持与操作系统/硬件耦合度最低。
///
/// # 契约说明（What）
/// - **前置条件**：实现必须同时满足 [`TaskExecutor`] 与 [`TimeDriver`] 的契约，并具备线程安全性。
/// - **后置条件**：通过该 trait 注入 [`CoreServices`] 后，上层可假定任务调度与计时功能随时可用。
///
/// # 风险提示（Trade-offs）
/// - 该 trait 不额外声明新方法，便于以 `dyn AsyncRuntime` 作为统一注入入口；如需扩展请定义包裹结构体。
pub trait AsyncRuntime:
    TaskExecutor + TimeDriver + Send + Sync + 'static + crate::sealed::Sealed
{
}

impl<T> AsyncRuntime for T where T: TaskExecutor + TimeDriver + Send + Sync + 'static {}
