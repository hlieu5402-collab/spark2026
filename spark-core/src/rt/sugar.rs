use alloc::{borrow::Cow, boxed::Box};
use core::{any::Any, future::Future};

use crate::{
    contract, pipeline,
    runtime::{self, JoinHandle, TaskError, TaskExecutor},
};

/// `RuntimeCaps` 描述“可以提交任务”的运行时能力集合。
///
/// # 设计动机（Why）
/// - 运行时代码可能来自宿主提供的 `AsyncRuntime`、Pipeline `Context` 或测试桩实现；
///   统一的 trait 能避免在语法糖中重复编写样板。
/// - `spawn_with` 将“取出执行器 + 提交任务 + 获得句柄”这组操作抽象为一次调用，
///   让上层 API 专注于业务逻辑。
///
/// # 契约说明（What）
/// - **前置条件**：实现者需保证在 `spawn_with` 调用期间运行时处于可用状态；
/// - **返回值**：`Join<T>` 应满足 `Send + 'static`，并遵循 [`TaskHandle`](runtime::TaskHandle) 契约。
///
/// # 风险提示（Trade-offs）
/// - Trait 不会自动克隆 [`CallContext`](contract::CallContext)；若 Future 需要所有权请显式调用
///   [`CallContext::clone_call`](CallContext::clone_call)；
/// - 若运行时返回自定义句柄，请在实现文档中说明与 `JoinHandle` 的差异，避免误用。
pub trait RuntimeCaps {
    /// 运行时返回的 Join 句柄类型。
    type Join<T: Send + 'static>: Send + 'static;

    /// 在给定上下文与 Future 的情况下提交任务。
    fn spawn_with<F>(&self, ctx: &contract::CallContext, fut: F) -> Self::Join<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static;
}

/// 对任意实现 [`TaskExecutor`] 且满足 `Sized` 的类型提供默认实现。
impl<T> RuntimeCaps for T
where
    T: TaskExecutor,
{
    type Join<U: Send + 'static> = JoinHandle<U>;

    fn spawn_with<F>(&self, ctx: &contract::CallContext, fut: F) -> Self::Join<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        TaskExecutor::spawn(self, ctx, fut)
    }
}

/// `ContextCaps` 将 Pipeline [`Context`](pipeline::Context) 适配为 [`RuntimeCaps`]。
#[derive(Clone, Copy)]
pub struct ContextCaps<'a, C: pipeline::Context + ?Sized> {
    context: &'a C,
}

impl<'a, C: pipeline::Context + ?Sized> ContextCaps<'a, C> {
    /// 构造基于 Pipeline `Context` 的运行时能力视图。
    pub fn new(context: &'a C) -> Self {
        Self { context }
    }

    /// 返回底层 Pipeline `Context` 引用，便于访问其它控制面能力。
    pub fn context(&self) -> &'a C {
        self.context
    }
}

impl<'a, C> RuntimeCaps for ContextCaps<'a, C>
where
    C: pipeline::Context + ?Sized,
{
    type Join<T: Send + 'static> = JoinHandle<T>;

    fn spawn_with<F>(&self, ctx: &contract::CallContext, fut: F) -> Self::Join<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let erased = async move {
            let value = fut.await;
            Ok::<Box<dyn Any + Send>, TaskError>(Box::new(value))
        };
        let handle = self.context.executor().spawn_dyn(ctx, Box::pin(erased));
        map_join(handle)
    }
}

/// `BorrowedRuntimeCaps` 为任意已实现 [`RuntimeCaps`] 的类型提供借用包装。
///
/// - 适用于只持有运行时能力引用的场景，避免为了满足 `RuntimeCaps` 而自定义新结构。
#[derive(Clone, Copy)]
pub struct BorrowedRuntimeCaps<'a, R: RuntimeCaps + ?Sized> {
    inner: &'a R,
}

impl<'a, R: RuntimeCaps + ?Sized> BorrowedRuntimeCaps<'a, R> {
    /// 构造引用包装。
    pub fn new(inner: &'a R) -> Self {
        Self { inner }
    }

    /// 返回底层运行时能力引用。
    pub fn inner(&self) -> &'a R {
        self.inner
    }
}

impl<'a, R: RuntimeCaps + ?Sized> RuntimeCaps for BorrowedRuntimeCaps<'a, R> {
    type Join<T: Send + 'static> = R::Join<T>;

    fn spawn_with<F>(&self, ctx: &contract::CallContext, fut: F) -> Self::Join<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.inner.spawn_with(ctx, fut)
    }
}

/// `CallContext` 语法糖视图，将 [`contract::CallContext`] 与 [`RuntimeCaps`] 绑定在一起。
pub struct CallContext<'a, R: RuntimeCaps> {
    call: &'a contract::CallContext,
    runtime_caps: R,
}

impl<'a, R: RuntimeCaps> CallContext<'a, R> {
    /// 构造语法糖上下文视图。
    pub fn new(call: &'a contract::CallContext, runtime_caps: R) -> Self {
        Self { call, runtime_caps }
    }

    /// 返回底层调用上下文引用。
    pub fn call(&self) -> &'a contract::CallContext {
        self.call
    }

    /// 克隆底层上下文，获取拥有所有权的副本。
    pub fn clone_call(&self) -> contract::CallContext {
        self.call.clone()
    }

    /// 访问运行时能力对象。
    pub fn runtime_caps(&self) -> &R {
        &self.runtime_caps
    }

    /// 拆分结构，返回底层引用与运行时能力。
    pub fn into_parts(self) -> (&'a contract::CallContext, R) {
        (self.call, self.runtime_caps)
    }

    /// 基于引用构造语法糖上下文。
    pub fn borrowed<RC>(
        call: &'a contract::CallContext,
        runtime_caps: &'a RC,
    ) -> CallContext<'a, BorrowedRuntimeCaps<'a, RC>>
    where
        RC: RuntimeCaps + ?Sized,
    {
        CallContext::new(call, BorrowedRuntimeCaps::new(runtime_caps))
    }
}

/// 基于语法糖上下文提交任务，返回运行时对应的 Join 句柄。
pub fn spawn_in<'a, R, F>(ctx: &CallContext<'a, R>, fut: F) -> R::Join<F::Output>
where
    R: RuntimeCaps,
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    ctx.runtime_caps.spawn_with(ctx.call, fut)
}

/// 让 [`runtime::CoreServices`] 直接具备 `RuntimeCaps` 能力，方便在依赖注入容器上调用语法糖。
impl RuntimeCaps for runtime::CoreServices {
    type Join<T: Send + 'static> = JoinHandle<T>;

    fn spawn_with<F>(&self, ctx: &contract::CallContext, fut: F) -> Self::Join<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let erased = async move {
            let value = fut.await;
            Ok::<Box<dyn Any + Send>, TaskError>(Box::new(value))
        };
        let handle = self.runtime().spawn_dyn(ctx, Box::pin(erased));
        map_join(handle)
    }
}

fn map_join<T>(handle: JoinHandle<Box<dyn Any + Send>>) -> JoinHandle<T>
where
    T: Send + 'static,
{
    handle.map(|result| {
        result.and_then(|boxed| {
            boxed
                .downcast::<T>()
                .map(|value| *value)
                .map_err(|_| TaskError::Failed(Cow::from("join handle type mismatch")))
        })
    })
}
