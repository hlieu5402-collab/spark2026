use alloc::{borrow::Cow, boxed::Box};
use core::{any::Any, future::Future};

use super::task::{JoinHandle, TaskError, TaskResult};
use crate::{BoxFuture, contract::CallContext, sealed::Sealed};

/// `TaskExecutor` 定义运行时的任务调度契约。
///
/// # 设计背景（Why）
/// - T21 任务要求从契约层彻底禁止“无上下文”的异步任务，以保证取消、截止与预算信号能够跨层传播；
/// - 因此 `spawn` 现需显式接收 [`CallContext`] 引用，运行时可以在此处记录、复制或转换上下文信息，再将其注入到具体的执行模型中。
///
/// # 逻辑解析（How）
/// - `spawn` 接受 `Future`，要求其 `Send + 'static`，以满足跨线程调度；
/// - 执行器需根据传入的 `ctx` 创建子上下文或传播取消令牌，并返回 [`JoinHandle`] 用于后续的状态管理与结果等待。
///
/// # 契约说明（What）
/// - **前置条件**：实现者必须确保对 `ctx` 的引用在任务入队期间保持有效，必要时应立即克隆或复制所需字段；
/// - **返回值**：`JoinHandle<F::Output>`，其中 `F::Output` 在成功完成时作为任务返回值，失败则统一映射为 [`TaskError`](super::task::TaskError)。
/// - **后置条件**：运行时应保证 `JoinHandle::join` 完成时任务已经结束，且若实现了取消协作，`ctx.cancellation()` 的状态与任务执行相互一致。
///
/// # 风险提示（Trade-offs）
/// - 强制要求上下文引用意味着无法再直接调用诸如 Tokio 提供的裸 `spawn` 接口，宿主实现需要在内部处理上下文传播；
/// - 若执行器选择忽略 `ctx`，虽然编译通过，但将违背契约预期，应在实现层提供合规的传播策略（例如克隆取消令牌、继承预算等）。
pub trait TaskExecutor: Send + Sync + 'static + Sealed {
    /// 对象安全的任务提交接口，使用类型擦除后的 `Box<dyn Any + Send>` 承载返回值。
    fn spawn_dyn(
        &self,
        ctx: &CallContext,
        fut: BoxFuture<'static, TaskResult<Box<dyn Any + Send>>>,
    ) -> JoinHandle<Box<dyn Any + Send>>;

    /// 泛型化的任务提交入口，便于直接获得带类型的 [`JoinHandle`]。
    ///
    /// # 设计意图（Why）
    /// - 契约层需要以最小成本推广“上下文必须随任务传播”的理念，
    ///   因此在对象安全接口外额外暴露默认实现，方便日常调用直接向下传递 [`CallContext`]；
    /// - 通过默认实现调用 [`TaskExecutor::spawn_dyn`]，既复用统一的类型擦除逻辑，
    ///   又允许宿主按需覆写以支持特定的优化（如自定义 join 句柄映射）。
    ///
    /// # 执行逻辑（How）
    /// - 将 `Future` 包装为类型擦除的 `BoxFuture` 并委托给 [`TaskExecutor::spawn_dyn`]；
    /// - 通过 [`JoinHandle::map`](super::task::JoinHandle::map) 在任务完成时恢复原始输出类型，
    ///   若 `downcast` 失败则返回 `TaskError::Failed` 以表明实现违背了泛型契约。
    ///
    /// # 契约说明（What）
    /// - **参数**：
    ///   - `ctx`：父调用上下文，要求实现方在提交任务时保留取消/截止/预算等信号；
    ///   - `fut`：待执行的异步任务，必须满足 `Send + 'static` 以支持跨线程调度；
    /// - **返回值**：
    ///   - 返回带类型的 [`JoinHandle<F::Output>`]，调用 `join` 后可得到任务结果或 [`TaskError`]；
    /// - **前置条件**：
    ///   - 仅当实现类型满足 `Self: Sized`（默认实现所需）时可直接使用该方法；
    ///   - `CallContext` 的生命周期需覆盖任务入队阶段；
    /// - **后置条件**：
    ///   - 若返回的 `JoinHandle` 完成，任务必定已经退出；
    ///   - 当实现遵循协作取消语义时，`ctx.cancellation()` 的状态应与任务协同。
    ///
    /// # 风险提示（Trade-offs & Gotchas）
    /// - 默认实现依赖类型擦除与 `downcast`，在高频场景可能产生细微的分配与 RTTI 成本；
    ///   若性能敏感，可在具体执行器中覆写此方法以避免装箱；
    /// - 若调用方意外传入与 `spawn_dyn` 不一致的句柄实现，将在 `downcast` 处触发失败，
    ///   因此建议保持默认实现或在覆写版本中同步调整类型擦除策略。
    fn spawn<F>(&self, ctx: &CallContext, fut: F) -> JoinHandle<F::Output>
    where
        Self: Sized,
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let erased = async move {
            let value = fut.await;
            Ok::<Box<dyn Any + Send>, TaskError>(Box::new(value))
        };
        let handle = self.spawn_dyn(ctx, Box::pin(erased));
        handle.map(|result| {
            result.and_then(|boxed| {
                boxed
                    .downcast::<F::Output>()
                    .map(|value| *value)
                    .map_err(|_| TaskError::Failed(Cow::from("join handle type mismatch")))
            })
        })
    }
}
