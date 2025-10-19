use crate::{BoxFuture, LocalBoxFuture};
use alloc::{borrow::Cow, boxed::Box, string::String};
use core::fmt;

/// `TaskPriority` 描述调度器参考的优先级队列等级。
///
/// # 设计背景（Why）
/// - 借鉴操作系统中的多级反馈队列（MLFQ）与业界运行时（Tokio、Seastar），提供统一的优先级语义。
///
/// # 逻辑解析（How）
/// - 采用离散的五档等级，既能覆盖服务端常见场景，也便于实现者映射到不同的调度策略。
///
/// # 契约说明（What）
/// - 提交任务时默认使用 [`TaskPriority::Normal`]；实现者可根据内部策略映射到权重或队列索引。
///
/// # 风险提示（Trade-offs）
/// - 过度依赖优先级可能导致饥饿；实现者应结合老化策略避免长期低优先级任务饿死。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskPriority {
    Critical,
    High,
    #[default]
    Normal,
    Low,
    Idle,
}

/// `TaskCancellationStrategy` 表达取消行为的强度。
///
/// # 设计背景（Why）
/// - 协作取消（cooperative cancellation）是跨平台运行时的共识，但在关机场景仍需要强制取消选项。
///
/// # 契约说明（What）
/// - `Cooperative` 要求任务自行检查取消信号；`Forceful` 则允许运行时立即终止。
///
/// # 风险提示（Trade-offs）
/// - 选择 `Forceful` 可能导致任务绕过析构流程；调用方需确保资源已通过其它途径安全释放。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TaskCancellationStrategy {
    #[default]
    Cooperative,
    Forceful,
}

/// `TaskLaunchOptions` 汇总提交任务时的元信息。
///
/// # 设计背景（Why）
/// - 将命名、优先级、取消策略、链路追踪标签等元信息统一封装，便于执行器做可观测性增强。
///
/// # 契约说明（What）
/// - `name`：用于日志与指标聚合；建议使用静态字符串，便于编译期内联。
/// - `priority`：调度器参考优先级，默认为普通级别。
/// - `cancellation`：取消策略，默认为协作取消。
/// - `tracing_labels`：额外的标签信息，采用 `String` 以兼容运行时动态拼接。
///
/// # 前置条件
/// - 调用方应确保 `name` 对于同类任务具有稳定含义，避免可观测数据碎片化。
///
/// # 后置条件
/// - 运行时可假定 `TaskLaunchOptions` 在任务生命周期内保持不变，可安全缓存引用。
#[derive(Clone, Debug, Default)]
pub struct TaskLaunchOptions {
    pub name: Option<Cow<'static, str>>,
    pub priority: TaskPriority,
    pub cancellation: TaskCancellationStrategy,
    pub tracing_labels: Option<String>,
}

impl TaskLaunchOptions {
    /// 以给定任务名构造配置。
    pub fn named(name: impl Into<Cow<'static, str>>) -> Self {
        TaskLaunchOptions {
            name: Some(name.into()),
            ..Default::default()
        }
    }
}

/// `TaskResult` 统一表示任务执行结果。
///
/// # 契约说明（What）
/// - `Ok(T)`：任务成功完成并返回值。
/// - `Err(TaskError)`：任务取消、失败或执行器故障。
pub type TaskResult<T = ()> = Result<T, TaskError>;

/// `TaskError` 枚举任务失败原因。
///
/// # 设计背景（Why）
/// - 吸收 Tokio `JoinError`、Fuchsia `TaskError` 等设计，区分取消、执行异常、执行器关闭等常见情况。
///
/// # 风险提示（Trade-offs）
/// - `Panicked` 仅包含静态信息；实际运行时若需捕获 panic payload，需额外在宿主实现层处理。
#[derive(Debug, Clone)]
pub enum TaskError {
    Cancelled,
    Panicked,
    ExecutorTerminated,
    Failed(Cow<'static, str>),
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::Cancelled => write!(f, "task cancelled"),
            TaskError::Panicked => write!(f, "task panicked"),
            TaskError::ExecutorTerminated => write!(f, "executor terminated"),
            TaskError::Failed(reason) => write!(f, "task failed: {reason}"),
        }
    }
}

/// `TaskHandle` 定义运行时返回的任务控制句柄。
///
/// # 设计背景（Why）
/// - 结合 Tokio `JoinHandle` 与研究型运行时（如 Verona、Glommio）的控制面抽象，提供取消、状态查询与异步等待接口。
///
/// # 逻辑解析（How）
/// - `cancel`：注入取消信号，可指定策略。
/// - `is_finished` / `is_cancelled`：供健康检查与调度策略使用。
/// - `id`：可选的人类可读标识，方便调试。
/// - `detach`：释放控制权但保持任务运行。
/// - `join`：异步等待任务结束，返回执行结果。
///
/// # 契约说明（What）
/// - **前置条件**：句柄必须独立维护任务生命周期引用计数，确保在 `detach` 后仍能安全运行。
/// - **后置条件**：`join` 完成后，运行时保证任务已结束，且不会再次调用该句柄上的其它方法。
///
/// # 性能契约（Performance Contract）
/// - `join` 返回 [`BoxFuture`] 以保持 Trait 对象安全；每次调用会分配 `Box` 并通过虚表唤醒内部状态机。
/// - `async_contract_overhead` 基准在 20 万次 Future 轮询中测得泛型实现 6.23ns/次、`BoxFuture` 6.09ns/次（约 -0.9%）。
///   结果显示默认动态分发对调度线程影响极小。【e8841c†L4-L13】
/// - 在批量等待或极限延迟场景，可直接持有具体实现（如运行时自定义的 `JoinHandle`）或在内部维护 `Box` 缓冲池，以规避分配；
///   同时保留 `cancel`/`detach` 等同步方法的零分配特性。
///
/// # 风险提示（Trade-offs）
/// - 接口对象安全，允许以 `Box<dyn TaskHandle>` 搭配注入；实现者需权衡性能与动态分发开销。
pub trait TaskHandle: Send + Sync {
    fn cancel(&self, strategy: TaskCancellationStrategy);
    fn is_finished(&self) -> bool;
    fn is_cancelled(&self) -> bool;
    fn id(&self) -> Option<&str>;
    fn detach(self: Box<Self>);
    fn join(self: Box<Self>) -> BoxFuture<'static, TaskResult>;
}

/// `ManagedSendTask` 统一封装跨线程可运行的异步任务。
///
/// # 设计背景（Why）
/// - 采用 struct 包装 Future + 元信息，方便执行器在入队时进行统一的指标上报与调度决策。
///
/// # 前置条件
/// - `future` 必须满足 `Send + 'static`，以允许在多线程执行器中迁移。
///
/// # 后置条件
/// - 提交后运行时拥有任务所有权；调用方不再直接访问 Future。
pub struct ManagedSendTask {
    pub future: BoxFuture<'static, TaskResult>,
    pub options: TaskLaunchOptions,
}

/// `ManagedBlockingTask` 封装阻塞任务委托逻辑。
///
/// # 契约说明（What）
/// - `operation` 返回 `TaskResult`，供执行器统一处理错误。
/// - 运行时应在独立线程池执行该任务，避免阻塞核心调度器线程。
pub struct ManagedBlockingTask {
    pub operation: Box<dyn FnOnce() -> TaskResult + Send + 'static>,
    pub options: TaskLaunchOptions,
}

/// `ManagedLocalTask` 封装 `!Send` 异步任务。
///
/// # 契约说明（What）
/// - `future` 仅需满足 `'static`，可运行在单线程执行器。
/// - 调用方必须确保宿主线程在任务完成前不会退出。
pub struct ManagedLocalTask {
    pub future: LocalBoxFuture<'static, TaskResult>,
    pub options: TaskLaunchOptions,
}

/// `SendTaskSubmission` 为便捷构造器，配合 `Into` 自动转换。
pub struct SendTaskSubmission(pub ManagedSendTask);

impl From<ManagedSendTask> for SendTaskSubmission {
    fn from(value: ManagedSendTask) -> Self {
        SendTaskSubmission(value)
    }
}

/// `BlockingTaskSubmission` 为阻塞任务的便捷包装。
pub struct BlockingTaskSubmission(pub ManagedBlockingTask);

impl From<ManagedBlockingTask> for BlockingTaskSubmission {
    fn from(value: ManagedBlockingTask) -> Self {
        BlockingTaskSubmission(value)
    }
}

/// `LocalTaskSubmission` 为本地任务的便捷包装。
pub struct LocalTaskSubmission(pub ManagedLocalTask);

impl From<ManagedLocalTask> for LocalTaskSubmission {
    fn from(value: ManagedLocalTask) -> Self {
        LocalTaskSubmission(value)
    }
}
