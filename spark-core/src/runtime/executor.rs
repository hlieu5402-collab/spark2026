use super::task::{BlockingTaskSubmission, LocalTaskSubmission, SendTaskSubmission, TaskHandle};
use alloc::boxed::Box;

/// `TaskExecutor` 定义运行时的任务调度契约。
///
/// # 设计背景（Why）
/// - 综合业界稳定运行时（Tokio、Actix、SeaStar）与研究前沿（可预测调度、分层执行器），
///   将任务提交拆分为三类：跨线程异步、阻塞委托、本地线程任务。
///
/// # 逻辑解析（How）
/// - `spawn`：提交跨线程异步任务，返回 [`TaskHandle`] 以供取消/等待。
/// - `spawn_blocking`：将阻塞计算卸载至后台线程池，避免拖慢 I/O 驱动线程。
/// - `spawn_local`：用于 `!Send` 任务，通常绑定到事件循环所在线程。
/// - `spawn_detached` / `spawn_blocking_detached` / `spawn_local_detached`：默认实现基于句柄 `detach`，
///   方便无需监听结果的调用方。
///
/// # 契约说明（What）
/// - **前置条件**：提交的任务必须满足相应的 Send/Sync 约束；阻塞任务需保证不会长期占用线程池。
/// - **后置条件**：返回的 [`TaskHandle`] 在任务结束后必定进入 `finished` 状态，调用方可安全丢弃。
///
/// # 风险提示（Trade-offs）
/// - 对象安全接口牺牲部分泛型性能，但换取运行时注入灵活性；对极端性能敏感场景可在宿主侧提供特化接口。
pub trait TaskExecutor: Send + Sync + 'static {
    /// 提交一个跨线程可调度的异步任务。
    fn spawn(&self, task: SendTaskSubmission) -> Box<dyn TaskHandle>;

    /// 以分离方式提交异步任务。
    fn spawn_detached(&self, task: SendTaskSubmission) {
        self.spawn(task).detach();
    }

    /// 提交一个阻塞任务，由运行时在线程池执行。
    fn spawn_blocking(&self, task: BlockingTaskSubmission) -> Box<dyn TaskHandle>;

    /// 以分离方式提交阻塞任务。
    fn spawn_blocking_detached(&self, task: BlockingTaskSubmission) {
        self.spawn_blocking(task).detach();
    }

    /// 提交 `!Send` 的本地异步任务。
    fn spawn_local(&self, task: LocalTaskSubmission) -> Box<dyn TaskHandle>;

    /// 以分离方式提交本地任务。
    fn spawn_local_detached(&self, task: LocalTaskSubmission) {
        self.spawn_local(task).detach();
    }
}
