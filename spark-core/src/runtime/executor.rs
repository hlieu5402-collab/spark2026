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
/// - 强制要求上下文引用意味着无法再直接调用诸如 `tokio::spawn` 的裸接口，宿主实现需要在内部处理上下文传播；
/// - 若执行器选择忽略 `ctx`，虽然编译通过，但将违背契约预期，应在实现层提供合规的传播策略（例如克隆取消令牌、继承预算等）。
pub trait TaskExecutor: Send + Sync + 'static + Sealed {
    /// 对象安全的任务提交接口，使用类型擦除后的 `Box<dyn Any + Send>` 承载返回值。
    fn spawn_dyn(
        &self,
        ctx: &CallContext,
        fut: BoxFuture<'static, TaskResult<Box<dyn Any + Send>>>,
    ) -> JoinHandle<Box<dyn Any + Send>>;
}

/// 为方便使用提供泛型适配层：开发者可直接调用 `spawn` 获得带类型的 [`JoinHandle`]。
pub trait TaskExecutorExt: TaskExecutor {
    fn spawn<F>(&self, ctx: &CallContext, fut: F) -> JoinHandle<F::Output>
    where
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

impl<T> TaskExecutorExt for T where T: TaskExecutor + ?Sized {}
