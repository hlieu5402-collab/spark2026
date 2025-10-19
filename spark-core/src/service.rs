use crate::Error;
use crate::status::ready::PollReady;
use core::future::Future;
use core::task::Context as TaskContext;

/// `Service` 抽象 L7 业务逻辑处理流程。
///
/// # 设计背景（Why）
/// - 借鉴 Tower 模型，将请求处理拆分为可组合的服务与中间件，统一 RPC、HTTP 等协议栈。
/// - `spark-core` 仅定义契约，具体实现可由宿主根据不同运行时编写。
///
/// # 契约说明（What）
/// - `Request` 泛型代表输入消息类型；`Response` 表示输出类型。
/// - `Error` 需实现 `crate::Error`，以融入统一错误链。
/// - `Future` 关联类型返回异步结果，必须满足 `Send + 'static` 以便跨线程调度。
///
/// # 风险提示（Trade-offs）
/// - 若内部使用 `async fn` 实现，需要 `BoxFuture` 或 GAT 支持；宿主可结合 `async_trait` 等宏完成。
pub trait Service<Request> {
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

    /// 处理请求并返回异步响应。
    fn call(&mut self, req: Request) -> Self::Future;
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
