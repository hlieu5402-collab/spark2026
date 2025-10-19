use crate::Error;
use crate::status::ready::PollReady;
use core::future::Future;
use core::task::Context as TaskContext;
use crate::{
    Error, SparkError,
    backpressure::PollReady,
    buffer::PipelineMessage,
    contract::{CallContext, CloseReason},
    future::BoxFuture,
};
use alloc::{boxed::Box, sync::Arc};
use core::{
    future::Future,
    task::{Context as TaskContext, Poll},
};

/// `Service` 抽象 L7 业务逻辑处理流程。
///
/// # 设计背景（Why）
/// - 借鉴 Tower 模型，将请求处理拆分为可组合的服务与中间件，统一 RPC、HTTP 等协议栈。
/// - `Service` 提供泛型零成本接口；[`DynService`] 则面向插件、脚本等场景提供对象安全能力，二者在语义上等价，满足“双层 API 模型”。
/// - `CallContext` 将取消/截止/预算三元组、安全策略、可观测性契约一并携带，确保所有公共方法显式接纳统一上下文。
///
/// # 契约说明（What）
/// - `Request` 泛型代表输入消息类型；`Response` 表示输出类型。
/// - `Error` 需实现 [`crate::Error`]，以融入统一错误链与三层错误域模型。
/// - `Future` 关联类型返回异步结果，必须满足 `Send + 'static` 以便跨线程调度。
/// - `poll_ready` 必须返回 [`PollReady`]，用于统一背压语义；若预算耗尽应返回 `PollReady::BudgetExhausted`。
/// - `call` 需消耗一个 [`CallContext`]（可克隆），以便在异步链路中延续取消与安全信息。
///
/// # 风险提示（Trade-offs）
/// - 若内部使用 `async fn` 实现，需要 `BoxFuture` 或 GAT 支持；宿主可结合 `async_trait` 等宏完成。
/// - `CallContext` 内部包含 [`Arc`]，克隆成本为常数，但仍应避免在热路径中不必要的复制。
pub trait Service<Request>: Send + Sync + 'static {
    /// 响应类型。
    type Response;
    /// 错误类型，必须兼容 `spark-core` 错误模型。
    type Error: Error;
    /// 异步返回类型。
    type Future: Future<Output = Result<Self::Response, Self::Error>> + Send + 'static;

    /// 检查服务是否准备好处理请求。
    ///
    /// # 契约说明（What）
    /// - 返回 [`PollReady<Self::Error>`]：
    ///   - `Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))`：服务可立即处理请求；
    ///   - `Poll::Ready(ReadyCheck::Ready(ReadyState::Busy(_)))`：服务繁忙并附带原因；
    ///   - `Poll::Ready(ReadyCheck::Ready(ReadyState::BudgetExhausted(_)))`：调用预算已耗尽；
    ///   - `Poll::Ready(ReadyCheck::Ready(ReadyState::RetryAfter(_)))`：建议等待后重试；
    ///   - `Poll::Ready(ReadyCheck::Err(_))`：检查发生错误；
    ///   - `Poll::Pending`：仍在等待就绪。
    ///
    /// # 前置条件（Contract）
    /// - 调用方必须在 `call` 之前反复驱动该方法，直至获得 `ReadyState::Ready` 或选择根据繁忙状态退避。
    ///
    /// # 后置条件（Contract）
    /// - 当返回 `ReadyState::Ready` 时，服务保证下一次 `call` 可安全执行；
    /// - 若返回 `Busy`/`BudgetExhausted`，服务仅承诺保持内部状态一致，调用方需根据业务策略退避或告警。
    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> PollReady<Self::Error>;
    fn poll_ready(
        &mut self,
        ctx: &CallContext,
        cx: &mut TaskContext<'_>,
    ) -> Poll<PollReady<Self::Error>>;

    /// 处理请求并返回异步响应。
    fn call(&mut self, ctx: CallContext, req: Request) -> Self::Future;
}

/// `Layer` 抽象用于包装服务的中间件。
///
/// # 设计背景（Why）
/// - 支持将鉴权、重试、熔断等横切逻辑作为 Layer 组合。
///
/// # 契约说明（What）
/// - 通过显式指定 `Request` 类型，确保中间件与底层服务在输入输出上保持一致。
/// - `layer` 接受一个内部服务 `inner`，返回包裹后的新服务。
///
/// # 风险提示（Trade-offs）
/// - Layer 组合顺序会影响语义；建议实现者在文档中标注依赖关系。
pub trait Layer<S, Request>
where
    S: Service<Request>,
{
    /// 包裹后的服务类型。
    type Service: Service<Request, Response = S::Response, Error = S::Error>;

    /// 应用中间件并返回新服务。
    fn layer(&self, inner: S) -> Self::Service;
}

/// 对象安全的服务接口，供插件化或脚本运行时使用。
///
/// # 设计背景（Why）
/// - 多语言插件、动态脚本和热插拔场景无法在编译期确定请求/响应类型，需要基于 [`PipelineMessage`] 的对象擦除能力。
/// - 与泛型 [`Service`] 保持语义等价：背压、预算、错误链路完全一致，确保统一契约。
///
/// # 契约说明（What）
/// - `poll_ready` 使用 [`PollReady`] 表达背压；实现必须遵循预算耗尽即返回 `BudgetExhausted` 的约定。
/// - `call` 返回 [`BoxFuture`]，输出必须是 `PipelineMessage`，以在 Handler/Router 之间统一传递。
/// - `close_graceful` 与 `closed` 贯彻“优雅关闭契约”，禁止静默丢弃。
///
/// # 风险提示（Trade-offs）
/// - 对象安全接口带来一次虚表分发与堆分配；若场景可静态确定类型，应优先使用泛型 [`Service`] 以获得零成本调用。
pub trait DynService: Send + Sync {
    /// 检查服务是否就绪。
    fn poll_ready_dyn(
        &mut self,
        ctx: &CallContext,
        cx: &mut TaskContext<'_>,
    ) -> Poll<PollReady<SparkError>>;

    /// 消费消息并返回结果。
    fn call_dyn(
        &mut self,
        ctx: CallContext,
        req: PipelineMessage,
    ) -> BoxFuture<'static, Result<PipelineMessage, SparkError>>;

    /// 触发优雅关闭流程。
    fn graceful_close(&mut self, reason: CloseReason);

    /// 等待服务完全关闭。
    fn closed(&mut self) -> BoxFuture<'static, Result<(), SparkError>>;
}

/// 将泛型 `Service` 适配为对象安全 [`DynService`] 的桥接器。
///
/// # 设计背景（Why）
/// - 在 Router/Plugin 框架中需要动态调度服务，实现者可利用该适配器复用现有泛型实现。
/// - 通过显式的 `encode`/`decode` 闭包，调用方可定义请求/响应与 [`PipelineMessage`] 之间的映射，同时保持类型安全。
///
/// # 契约说明（What）
/// - `decode` 负责将 `PipelineMessage` 转换为泛型请求；失败应返回结构化的 [`SparkError`]。
/// - `encode` 将泛型响应转换回 `PipelineMessage`；必须遵循零拷贝优先原则，避免不必要的复制。
/// - `close_graceful` 默认转发至内部服务的 `Drop`/自定义逻辑；若需要自定义关闭流程，可在构造时提供 `on_close` 钩子。
///
/// # 风险提示（Trade-offs）
/// - 适配器内部会克隆 `CallContext`（`Arc` 语义），若在极端高频场景下仍可接受；如需进一步优化，可自定义专用适配器。
type CloseCallback<S> = dyn Fn(&mut S, CloseReason) + Send + Sync;

pub struct ServiceObject<S, Request, Response, Decode, Encode>
where
    S: Service<Request, Response = Response, Error = SparkError>,
    Decode: Fn(PipelineMessage) -> Result<Request, SparkError> + Send + Sync + 'static,
    Encode: Fn(Response) -> PipelineMessage + Send + Sync + 'static,
{
    inner: S,
    decode: Arc<Decode>,
    encode: Arc<Encode>,
    on_close: Arc<CloseCallback<S>>,
}

impl<S, Request, Response, Decode, Encode> ServiceObject<S, Request, Response, Decode, Encode>
where
    S: Service<Request, Response = Response, Error = SparkError>,
    Decode: Fn(PipelineMessage) -> Result<Request, SparkError> + Send + Sync + 'static,
    Encode: Fn(Response) -> PipelineMessage + Send + Sync + 'static,
{
    /// 使用默认关闭钩子构建适配器。
    pub fn new(inner: S, decode: Decode, encode: Encode) -> Self {
        Self {
            inner,
            decode: Arc::new(decode),
            encode: Arc::new(encode),
            on_close: Arc::new(|_: &mut S, _: CloseReason| {}),
        }
    }

    /// 自定义关闭钩子，便于在对象层触发资源释放或审计。
    pub fn with_close_hook(
        mut self,
        hook: impl Fn(&mut S, CloseReason) + Send + Sync + 'static,
    ) -> Self {
        self.on_close = Arc::new(hook);
        self
    }
}

impl<S, Request, Response, Decode, Encode> DynService
    for ServiceObject<S, Request, Response, Decode, Encode>
where
    S: Service<Request, Response = Response, Error = SparkError>,
    Decode: Fn(PipelineMessage) -> Result<Request, SparkError> + Send + Sync + 'static,
    Encode: Fn(Response) -> PipelineMessage + Send + Sync + 'static,
{
    fn poll_ready_dyn(
        &mut self,
        ctx: &CallContext,
        cx: &mut TaskContext<'_>,
    ) -> Poll<PollReady<SparkError>> {
        self.inner.poll_ready(ctx, cx)
    }

    fn call_dyn(
        &mut self,
        ctx: CallContext,
        req: PipelineMessage,
    ) -> BoxFuture<'static, Result<PipelineMessage, SparkError>> {
        let decode = self.decode.clone();
        let encode = self.encode.clone();
        match (decode.as_ref())(req) {
            Ok(request) => {
                let fut = self.inner.call(ctx, request);
                Box::pin(async move {
                    let response = fut.await?;
                    Ok((encode.as_ref())(response))
                })
            }
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }

    fn graceful_close(&mut self, reason: CloseReason) {
        (self.on_close)(&mut self.inner, reason);
    }

    fn closed(&mut self) -> BoxFuture<'static, Result<(), SparkError>> {
        Box::pin(async { Ok(()) })
    }
}

/// 将 [`DynService`] 封装为可克隆句柄，便于在 Router 与 Pipeline 之间共享。
#[derive(Clone)]
pub struct BoxService {
    inner: Arc<dyn DynService>,
}

impl BoxService {
    /// 构造新的对象安全服务。
    pub fn new(inner: Arc<dyn DynService>) -> Self {
        Self { inner }
    }

    /// 访问内部 trait 对象。
    pub fn as_dyn(&self) -> &(dyn DynService) {
        &*self.inner
    }
}
