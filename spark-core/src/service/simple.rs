//! Service 快速原型工具箱：提供闭包转 Service 的胶水类型，以及过程宏可复用的就绪协调器。
//!
//! # 模块定位（Why）
//! - `SimpleServiceFn`：为轻量场景提供“闭包 -> Service”桥接，便于在测试、样例中快速起步；
//! - `SequentialService` + `AsyncFnLogic`：组合生成顺序执行的泛型 Service，实现 `#[spark::service]` 的核心；
//! - `ServiceReadyCoordinator`：封装 `poll_ready`/`call` 状态机，供过程宏在不分配堆内存的前提下实现顺序执行模型；
//! - `ReadyFutureGuard`/`GuardedFuture`：保证异步调用在正常完成或提前丢弃时都能唤醒等待者，避免背压锁死。
//!
//! # 使用指引（How）
//! - 业务代码若只需自定义 `call`，可直接使用 `SimpleServiceFn` 包裹闭包；
//! - `#[spark::service]` 宏会自动引用本模块中的协调器，实现开箱即用的 `poll_ready` 合约；
//! - 若需扩展更复杂的并发策略，可在保持 `ServiceReadyCoordinator` 语义不变的前提下另行封装。

use alloc::sync::Arc;
use core::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context as TaskContext, Poll, Waker},
};

use spin::Mutex;

use crate::{
    Error,
    context::ExecutionContext,
    contract::CallContext,
    service::Service,
    status::{PollReady, ReadyCheck, ReadyState},
};

/// 将闭包直接适配为 `Service` 的轻量封装器。
///
/// # 设计动机（Why）
/// - 在编写样例、测试或简单中间件时，往往只需一段异步闭包即可表达业务逻辑；
/// - 与 `tower::service_fn` 类似，本类型避免手写样板代码，便于快速验证契约；
/// - 仍遵循 `spark-core` 的错误抽象，使得测试代码与生产代码保持一致的错误链条。
///
/// # 行为描述（How）
/// - `poll_ready` 恒返回就绪，适用于对资源占用敏感度较低的无状态逻辑；
/// - `call` 直接执行闭包并返回其 `Future`，不会拦截或修改业务结果；
/// - 调用方如需更复杂的背压控制，可组合上层 Layer 或改用 `ServiceReadyCoordinator`。
///
/// # 契约说明（What）
/// - **输入**：闭包类型 `F` 接受 [`CallContext`] 与请求体，并返回实现 [`Future`] 的对象；
/// - **输出**：实现 [`Service`]，响应类型与错误类型由闭包决定；
/// - **前置条件**：闭包必须是 `Send + Sync + 'static`，返回的 Future 同样满足 `Send + 'static`；
/// - **后置条件**：每次调用互不影响，保持零状态保留（stateless）。
///
/// # 风险提示（Trade-offs & Gotchas）
/// - 恒定就绪意味着无法表达资源枯竭场景，适合自测或单元测试；
/// - 若闭包内部捕获非 `'static` 引用将导致编译失败，这是契约明确要求。
pub struct SimpleServiceFn<F>(pub F);

impl<F, Request, Fut, Response, E> Service<Request> for SimpleServiceFn<F>
where
    F: FnMut(CallContext, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, E>> + Send + 'static,
    E: Error,
{
    type Response = Response;
    type Error = E;
    type Future = Fut;

    fn poll_ready(
        &mut self,
        _ctx: &ExecutionContext<'_>,
        _cx: &mut TaskContext<'_>,
    ) -> PollReady<Self::Error> {
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    }

    fn call(&mut self, ctx: CallContext, req: Request) -> Self::Future {
        (self.0)(ctx, req)
    }
}

/// 描述一次 Service 调用的逻辑单元，使顺序执行器可以在类型层面表达 Future 的具体形态。
///
/// # 设计目标（Why）
/// - 将“如何生成业务 Future”与“如何管理就绪状态”解耦，便于过程宏与手写 Service 共用同一套执行器；
/// - 避免使用 `BoxFuture` 等分配型包装器，保持热路径零堆分配；
/// - 为未来扩展（如并行度控制）预留插拔点。
pub trait ServiceLogic<Request>: Send + Sync + 'static {
    /// 业务响应类型。
    type Response: Send + 'static;
    /// 业务错误类型，需实现 [`Error`]。
    type Error: Error + Send + 'static;
    /// 具体的 Future 类型，实现者可返回 `async fn` 编译生成的匿名 Future。
    type Future: Future<Output = Result<Self::Response, Self::Error>> + Send + 'static;

    /// 触发一次业务调用。
    ///
    /// # 契约说明
    /// - **输入**：完整的 [`CallContext`] 与请求体；
    /// - **输出**：代表本次调用的 Future；
    /// - **前置条件**：调用者需确保在获得就绪信号后再调用此函数。
    fn invoke(&mut self, ctx: CallContext, req: Request) -> Self::Future;
}

/// 将闭包适配为 [`ServiceLogic`]，供过程宏与测试复用。
pub struct AsyncFnLogic<F>(pub F);

impl<Request, F, Fut, Response, E> ServiceLogic<Request> for AsyncFnLogic<F>
where
    F: FnMut(CallContext, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, E>> + Send + 'static,
    Response: Send + 'static,
    E: Error + Send + 'static,
{
    type Response = Response;
    type Error = E;
    type Future = Fut;

    fn invoke(&mut self, ctx: CallContext, req: Request) -> Self::Future {
        (self.0)(ctx, req)
    }
}

/// 顺序执行器：结合 [`ServiceReadyCoordinator`] 与业务逻辑，生成合规的 `Service` 实现。
///
/// # 设计动机（Why）
/// - 过程宏展开时仅需注入闭包，即可获得完整的顺序执行语义；
/// - 将背压处理集中在协调器中，避免各处重复实现 waker 记录与唤醒。
pub struct SequentialService<L> {
    coordinator: ServiceReadyCoordinator,
    logic: L,
}

impl<L> SequentialService<L> {
    /// 构造顺序执行 Service。
    pub fn new(logic: L) -> Self {
        Self {
            coordinator: ServiceReadyCoordinator::new(),
            logic,
        }
    }
}

impl<L, Request> Service<Request> for SequentialService<L>
where
    L: ServiceLogic<Request>,
{
    type Response = L::Response;
    type Error = L::Error;
    type Future = GuardedFuture<L::Future, L::Response, L::Error>;

    fn poll_ready(
        &mut self,
        _ctx: &ExecutionContext<'_>,
        cx: &mut TaskContext<'_>,
    ) -> PollReady<Self::Error> {
        self.coordinator.poll_ready(cx)
    }

    fn call(&mut self, ctx: CallContext, req: Request) -> Self::Future {
        let guard = self.coordinator.begin_call();
        GuardedFuture::new(guard, self.logic.invoke(ctx, req))
    }
}

/// 将业务 Future 与 `ReadyFutureGuard` 绑定，在完成或 Drop 时自动恢复就绪状态。
///
/// # 设计动机（Why）
/// - 顺序执行模型要求任何请求结束后立刻唤醒等待的 waker，否则上游会永久卡在 `poll_ready`；
/// - 直接在业务 Future 中手动调用 `release` 容易遗漏错误分支，封装为 `GuardedFuture` 可集中处理。
///
/// # 行为描述（How）
/// - `new`：接收协调器守卫与业务 Future，并记录在内部；
/// - `poll`：转调内部 Future，一旦完成即调用 `ReadyFutureGuard::release`；
/// - `Drop`：若 Future 被提前丢弃，同样触发释放，保证状态不被悬挂。
///
/// # 契约说明（What）
/// - **输入**：实现 [`Future`] 的业务类型 `Fut`；
/// - **输出**：新的 Future，输出值与原始 `Fut` 保持一致；
/// - **前置条件**：创建者需保证守卫来自对应的协调器；
/// - **后置条件**：无论 `poll` 结果如何，协调器最终都会重新进入 `Ready` 状态。
#[doc(hidden)]
pub struct GuardedFuture<Fut, Response, Error> {
    guard: Option<ReadyFutureGuard>,
    future: Fut,
    _marker: PhantomData<fn() -> (Response, Error)>,
}

impl<Fut, Response, Error> GuardedFuture<Fut, Response, Error>
where
    Fut: Future<Output = Result<Response, Error>>,
{
    fn new(guard: ReadyFutureGuard, future: Fut) -> Self {
        Self {
            guard: Some(guard),
            future,
            _marker: PhantomData,
        }
    }
}

impl<Fut, Response, Error> Future for GuardedFuture<Fut, Response, Error>
where
    Fut: Future<Output = Result<Response, Error>>,
{
    type Output = Result<Response, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        match future.poll(cx) {
            Poll::Ready(output) => {
                if let Some(mut guard) = this.guard.take() {
                    guard.release();
                }
                Poll::Ready(output)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Fut, Response, Error> Drop for GuardedFuture<Fut, Response, Error> {
    fn drop(&mut self) {
        if let Some(mut guard) = self.guard.take() {
            guard.release();
        }
    }
}

/// `#[spark::service]` 宏复用的就绪协调器，实现顺序调用与 waker 管理。
///
/// # 设计意图（Why）
/// - 将 `poll_ready`/`call` 的状态迁移逻辑集中管理，避免宏展开重复生成易错代码；
/// - 通过 `Arc` + 原子位表示“是否可接收下一次请求”，避免堆分配和锁的额外开销；
/// - 提供明确的接口语义，便于未来扩展（如并行度计数、排队指标）。
///
/// # 行为细节（How）
/// - `poll_ready`：若可用直接返回 `Ready`，否则记录 waker 并返回 `Pending`；
/// - `begin_call`：原子地占用执行权，违反契约（未先 `poll_ready`）时直接 panic 提醒开发者；
/// - `release_once`：在调用完成或 Future 丢弃时唤醒等待者，恢复 `Ready` 状态。
///
/// # 契约说明（What）
/// - **前置条件**：`begin_call` 必须在最近一次 `poll_ready` 返回 Ready 之后调用；
/// - **后置条件**：`ReadyFutureGuard` 被释放后，协调器保证状态回到 Ready 并唤醒一个等待者；
/// - **边界**：同一时间仅允许一个请求进行中，体现过程宏的顺序执行模型。
#[doc(hidden)]
pub struct ServiceReadyCoordinator {
    inner: Arc<ServiceReadyState>,
}

impl ServiceReadyCoordinator {
    /// 创建新的协调器实例。
    ///
    /// # 实现要点（How）
    /// - 初始状态为 `Ready`，允许立即处理一次调用；
    /// - 使用 [`Arc`] 保证宏生成的 Future 在被丢弃时也能安全访问状态。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ServiceReadyState::new()),
        }
    }

    /// 在 `poll_ready` 中查询当前状态，并在必要时登记 waker。
    pub fn poll_ready<E>(&self, cx: &mut TaskContext<'_>) -> PollReady<E>
    where
        E: Error,
    {
        if self.inner.is_ready() {
            return Poll::Ready(ReadyCheck::Ready(ReadyState::Ready));
        }

        self.inner.register(cx.waker());

        if self.inner.is_ready() {
            Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
        } else {
            Poll::Pending
        }
    }

    /// 占用执行权并返回生命周期绑定的释放守卫。
    pub fn begin_call(&self) -> ReadyFutureGuard {
        if !self.inner.try_acquire() {
            panic!("ServiceReadyCoordinator::begin_call 必须在 poll_ready 返回 Ready 后调用");
        }
        ReadyFutureGuard::new(self.clone())
    }

    fn release_once(&self) {
        self.inner.release();
    }
}

impl Clone for ServiceReadyCoordinator {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for ServiceReadyCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// 在异步调用生命周期内保持占用，并在 Drop 时自动释放。
///
/// # 设计动机（Why）
/// - 保证无论 Future 正常完成、返回错误还是提前被取消，协调器状态都能回到 Ready；
/// - 将释放逻辑集中在 `Drop`，减少宏展开时对异常分支的重复代码。
///
/// # 行为说明（How）
/// - `release`：显式释放并阻止二次释放；
/// - `Drop`：若 Future 被提前丢弃，自动执行一次释放，确保 waker 被唤醒。
#[doc(hidden)]
pub struct ReadyFutureGuard {
    coordinator: ServiceReadyCoordinator,
    armed: bool,
}

impl ReadyFutureGuard {
    fn new(coordinator: ServiceReadyCoordinator) -> Self {
        Self {
            coordinator,
            armed: true,
        }
    }

    /// 显式释放执行权，允许下一次 `poll_ready` 返回就绪。
    pub fn release(&mut self) {
        if self.armed {
            self.coordinator.release_once();
            self.armed = false;
        }
    }
}

impl Drop for ReadyFutureGuard {
    fn drop(&mut self) {
        if self.armed {
            self.coordinator.release_once();
            self.armed = false;
        }
    }
}

struct ServiceReadyState {
    ready: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl ServiceReadyState {
    const fn new() -> Self {
        Self {
            ready: AtomicBool::new(true),
            waker: Mutex::new(None),
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn try_acquire(&self) -> bool {
        self.ready
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release(&self) {
        self.ready.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().take() {
            waker.wake();
        }
    }

    fn register(&self, waker: &Waker) {
        let mut slot = self.waker.lock();
        match slot.as_ref() {
            Some(existing) if existing.will_wake(waker) => {}
            _ => {
                *slot = Some(waker.clone());
            }
        }
    }
}
