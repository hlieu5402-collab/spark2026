use alloc::{boxed::Box, sync::Arc};
use core::fmt;

use crate::{
    SparkError,
    buffer::PipelineMessage,
    context::ExecutionContext,
    contract::{CallContext, CloseReason},
    future::BoxFuture,
    sealed::Sealed,
    status::ready::PollReady,
};

use super::generic::Service;

/// `DynService` 代表对象层的服务契约，用于插件、脚本等运行时动态扩展场景。
///
/// # 设计初衷（Why）
/// - 在运行时加载的 Handler/Router 组件中，需要统一的对象安全接口以存放在容器或注册表；
/// - 与泛型 [`Service`] 对应，实现“双层 API 等价”——所有语义（背压、预算、优雅关闭）完全保持一致；
/// - 避免额外依赖：仅引用 `PipelineMessage` 这一最小化的通用消息体，减少插件引入体积。
///
/// # 行为逻辑（How）
/// 1. `poll_ready_dyn` 调用底层实现的就绪检查，返回 [`PollReady<SparkError>`]；
/// 2. `call_dyn` 消费 `PipelineMessage` 并返回 `BoxFuture` 承载异步响应；
/// 3. `graceful_close`/`closed` 实现优雅关闭契约，便于运行时协调资源释放。
///
/// # 契约说明（What）
/// - **输入**：上下文与泛型层一致，均来源于 [`CallContext`] 的视图；
/// - **前置条件**：必须在 `call_dyn` 之前获得 `ReadyState::Ready`；
/// - **后置条件**：实现需保证在关闭流程中完成资源回收或返回明确错误。
///
/// # 风险提示（Trade-offs）
/// - 相较泛型层，多出一次虚表调用与一次堆分配；在热路径若可静态确定类型，仍建议使用 [`Service`]。
pub trait DynService: Send + Sync + Sealed {
    /// 检查对象层服务是否就绪。
    fn poll_ready_dyn(
        &mut self,
        ctx: &ExecutionContext<'_>,
        cx: &mut core::task::Context<'_>,
    ) -> PollReady<SparkError>;

    /// 调用对象层服务。
    fn call_dyn(
        &mut self,
        ctx: CallContext,
        req: PipelineMessage,
    ) -> BoxFuture<'static, Result<PipelineMessage, SparkError>>;

    /// 通知服务执行优雅关闭逻辑。
    fn graceful_close(&mut self, reason: CloseReason);

    /// 等待服务完全关闭。
    fn closed(&mut self) -> BoxFuture<'static, Result<(), SparkError>>;
}

/// 用于自定义关闭逻辑的钩子类型别名。
pub type CloseCallback<S> = dyn Fn(&mut S, CloseReason) + Send + Sync;

/// `ServiceObject` 将泛型 [`Service`] 适配为 [`DynService`]。
///
/// # 设计初衷（Why）
/// - 为控制面/插件系统提供统一的桥接器，避免重复编写类型擦除代码；
/// - 通过注入 `decode`/`encode` 闭包，将领域特定的数据模型转换为 `PipelineMessage`。
///
/// # 行为逻辑（How）
/// - 构造时保存 `inner` 泛型服务与转换闭包；
/// - `call_dyn` 解码消息、调用泛型服务、再编码响应；
/// - `graceful_close` 触发可选关闭钩子，方便释放外部资源。
///
/// # 契约说明（What）
/// - **泛型参数**：`Request`/`Response` 为泛型层业务类型；
/// - **前置条件**：`decode` 必须能够接受来自 Pipeline 的消息格式；
/// - **后置条件**：`encode` 需保证输出的 `PipelineMessage` 满足下游 Handler/Router 的期待。
///
/// # 风险提示（Trade-offs）
/// - `decode` 失败时直接返回错误，调用方需结合观测系统记录异常流量；
/// - 适配器内部克隆 `Arc` 闭包，若热路径频繁调用，可考虑自定义更轻量的适配器。
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
    /// 创建默认关闭钩子的适配器实例。
    pub fn new(inner: S, decode: Decode, encode: Encode) -> Self {
        Self {
            inner,
            decode: Arc::new(decode),
            encode: Arc::new(encode),
            on_close: Arc::new(|_: &mut S, _: CloseReason| {}),
        }
    }

    /// 指定额外的关闭钩子，用于释放外部资源或记录审计日志。
    pub fn with_close_hook(
        mut self,
        hook: impl Fn(&mut S, CloseReason) + Send + Sync + 'static,
    ) -> Self {
        self.on_close = Arc::new(hook);
        self
    }

    /// 访问内部泛型实现，便于测试或高级自定义。
    pub fn into_inner(self) -> S {
        self.inner
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
        ctx: &ExecutionContext<'_>,
        cx: &mut core::task::Context<'_>,
    ) -> PollReady<SparkError> {
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

/// `BoxService` 为对象层提供可克隆的服务句柄。
///
/// # 设计初衷（Why）
/// - Router 与 Pipeline 需要在多处共享对象层服务，实现 `Clone` 便于在多线程环境下分发；
/// - 内部使用 `Arc<dyn DynService>`，保证引用计数语义明确。
///
/// # 契约说明（What）
/// - **构造**：`new` 接收任意 `Arc<dyn DynService>`；
/// - **前置条件**：传入的 `Arc` 必须来源于有效的对象层实现；
/// - **后置条件**：克隆后的句柄共享相同的底层服务实例。
#[derive(Clone)]
pub struct BoxService {
    inner: Arc<dyn DynService>,
}

impl BoxService {
    /// 基于对象层实例构造句柄。
    pub fn new(inner: Arc<dyn DynService>) -> Self {
        Self { inner }
    }

    /// 以 trait 对象形式访问内部服务。
    pub fn as_dyn(&self) -> &(dyn DynService) {
        &*self.inner
    }
}

impl fmt::Debug for BoxService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}
