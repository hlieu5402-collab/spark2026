use alloc::{boxed::Box, format, sync::Arc, vec::Vec};

use spark_core::{
    CoreError, SparkError,
    buffer::PipelineMessage,
    error::codes,
    observability::{Logger, OwnedAttributeSet},
    pipeline::{Context, InboundHandler, middleware::MiddlewareDescriptor},
    router::{
        context::{RoutingContext, RoutingIntent, RoutingSnapshot},
        metadata::RouteMetadata,
        traits::object::{DynRouter, RouteDecisionObject},
    },
    runtime::TaskExecutor,
    service::BoxService,
    transport::intent::ConnectionIntent,
};

/// 描述构建 [`RoutingContext`] 所需的全部要素。
///
/// # 教案式说明
/// - **意图（Why）**：`RoutingContext` 要求引用型字段（意图、连接、动态元数据）在路由判定期间保持有效，
///   因此提前聚合这些所有权数据，便于在 Handler 内部安全借用。
/// - **结构（How）**：持有 [`RoutingIntent`]、可选的 [`ConnectionIntent`] 与动态 [`RouteMetadata`]；
///   Handler 将在调用 [`RoutingContext::new`] 时临时借用这些字段。
/// - **契约（What）**：
///   - `intent`：描述目标路由模式及调用方偏好；
///   - `connection`：可选的传输层意图，用于 QoS/安全策略协同；
///   - `dynamic_metadata`：运行时附带的标签，例如请求头或租户信息。
#[derive(Debug, Clone)]
pub struct RoutingContextParts {
    /// 路由意图，承载目标模式与偏好参数。
    pub intent: RoutingIntent,
    /// 可选的连接意图，用于路由决策结合网络因素。
    pub connection: Option<ConnectionIntent>,
    /// 动态路由元数据，供策略引擎读取。
    pub dynamic_metadata: RouteMetadata,
}

impl RoutingContextParts {
    /// 构造便捷函数，供上层在已知意图场景快速创建上下文材料。
    pub fn new(
        intent: RoutingIntent,
        connection: Option<ConnectionIntent>,
        dynamic_metadata: RouteMetadata,
    ) -> Self {
        Self {
            intent,
            connection,
            dynamic_metadata,
        }
    }
}

/// 为 `RouterHandler` 提供请求上下文所需材料的构造器契约。
///
/// # 教案式说明
/// - **意图（Why）**：不同接入层在 `PipelineMessage` 中承载上下文的方式各异，
///   通过扩展点让调用者自定义“如何从 `ctx` 与 `msg` 中抽取路由素材”，以避免 Handler 对具体协议写死假设。
/// - **逻辑（How）**：实现方可选择从 Channel 扩展、调用上下文或消息体中提取意图、连接与动态元数据，
///   并返回 [`RoutingContextParts`]；Handler 随后会将其装配成 [`RoutingContext`]。
/// - **契约（What）**：
///   - 输入为当前 Pipeline 上下文 `ctx`、待路由的消息 `msg` 及 Router 快照 `snapshot`；
///   - 成功时返回完整的 [`RoutingContextParts`]；失败时返回 [`SparkError`] 以便 Handler 记录并放弃处理。
pub trait RoutingContextBuilder: Send + Sync + 'static {
    /// 根据 Pipeline 状态与消息构建路由上下文所需材料。
    #[allow(clippy::result_large_err)]
    fn build(
        &self,
        ctx: &dyn Context,
        msg: &PipelineMessage,
        snapshot: RoutingSnapshot<'_>,
    ) -> spark_core::Result<RoutingContextParts, SparkError>;
}

/// 封装服务调用所需的调度句柄集合。
///
/// # 教案式说明
/// - **意图（Why）**：将执行器、调用上下文、写通道与日志器等多项依赖打包，
///   既便于 `spawn_service_task` 复用，也让参数列表控制在 Clippy 建议范围内。
/// - **逻辑（How）**：在 Handler 中就地构造 `ServiceDispatchContext`，
///   持有 `CallContext` 所有权，并借用执行器与通道指针；随后交由异步任务消费。
/// - **契约（What）**：
///   - `executor`：运行时执行器引用，必须保证在任务生命周期内有效；
///   - `call_ctx`：任务所属的调用上下文，要求克隆自当前 Pipeline；
///   - `channel`：写响应所需的通道引用，生命周期需满足 `'static` 假设；
///   - `logger`：用于记录任务执行失败的日志器指针；
///   - `trace`：追踪上下文，用于日志注入；
/// - **风险提示（Trade-offs）**：通道与日志器引用通过 `unsafe` 延长生命周期，
///   前提是控制器以 `Arc` 持有底层实现；若未来 HotSwapContext 改变所有权模型，需同步调整此结构。
struct ServiceDispatchContext<'exec> {
    executor: &'exec dyn TaskExecutor,
    call_ctx: spark_core::contract::CallContext,
    channel: &'static dyn spark_core::pipeline::channel::Channel,
    logger: &'static dyn Logger,
    trace: spark_core::observability::TraceContext,
}

/// `RouterHandler` 负责终止入站事件并将请求转交给对象层 Router。
///
/// # 教案式说明
/// - **意图（Why）**：将 Pipeline 最终的业务请求映射到 `DynRouter`，实现“Handler → Router → Service” 的桥接，
///   使得 Handler 链可以专注于编解码、鉴权等前置步骤。
/// - **逻辑（How）**：
///   1. 通过 [`RoutingContextBuilder`] 获取构建 `RoutingContext` 所需的材料；
///   2. 调用注入的 [`DynRouter::route_dyn`] 执行路由决策；
///   3. 将命中的 [`BoxService`] 托付给运行时执行器，异步调用业务逻辑；
///   4. 调用成功后写回响应，触发出站链路。
/// - **契约（What）**：
///   - Handler 不再调用 `ctx.forward_read`，意味着它必须位于入站链路尾部；
///   - 路由或服务调用失败会记录 ERROR 日志，但不会重复抛给后续 Handler。
/// - **风险（Trade-offs）**：
///   - 通过 `unsafe` 延长 `Channel`/`Logger` 引用生命周期以便在异步任务中使用，
///     假定 `HotSwapContext` 内部使用 `Arc` 持有这些资源；若未来上下文实现发生变化，需同步评估安全性。
pub struct RouterHandler {
    router: Arc<dyn DynRouter>,
    context_builder: Arc<dyn RoutingContextBuilder>,
    descriptor: MiddlewareDescriptor,
}

impl RouterHandler {
    /// 构造新的路由处理器实例。
    ///
    /// # 参数说明
    /// - `router`：对象层路由器实现，负责根据 `RoutingContext` 返回服务绑定；
    /// - `context_builder`：提取路由上下文材料的扩展点；
    /// - `descriptor`：用于链路 introspection 的元信息，如无特殊需求可传入 `MiddlewareDescriptor::new` 结果。
    pub fn new(
        router: Arc<dyn DynRouter>,
        context_builder: Arc<dyn RoutingContextBuilder>,
        descriptor: MiddlewareDescriptor,
    ) -> Self {
        Self {
            router,
            context_builder,
            descriptor,
        }
    }

    fn spawn_service_task(
        &self,
        dispatch: ServiceDispatchContext<'_>,
        service: BoxService,
        request: PipelineMessage,
    ) {
        let fut_ctx = dispatch.call_ctx.clone();
        let service_task = async move {
            let mut arc = service.into_arc();
            let dyn_service = Arc::get_mut(&mut arc).ok_or_else(|| {
                SparkError::new(
                    codes::APP_ROUTING_FAILED,
                    "router returned shared service instance; cannot obtain mutable access",
                )
            })?;

            let response = dyn_service.call_dyn(fut_ctx, request).await?;
            dispatch.channel.write(response).map(|_| ()).map_err(|err| {
                SparkError::new(codes::APP_ROUTING_FAILED, "failed to write response")
                    .with_cause(err)
            })
        };

        let trace_for_log = dispatch.trace;
        let log_future = async move {
            if let Err(err) = service_task.await {
                dispatch.logger.error(
                    "router-handler encountered error while invoking service",
                    Some(&err),
                    Some(&trace_for_log),
                );
            }

            Ok::<Box<dyn core::any::Any + Send>, spark_core::TaskError>(Box::new(()))
        };

        let handle = dispatch
            .executor
            .spawn_dyn(&dispatch.call_ctx, Box::pin(log_future));
        handle.detach();
    }

    fn handle_decision(
        &self,
        ctx: &dyn Context,
        decision: RouteDecisionObject,
        request: PipelineMessage,
        trace: spark_core::observability::TraceContext,
    ) {
        let binding = decision.binding().clone();
        let service = binding.service().clone();
        let call_ctx = ctx.call_context().clone();
        let executor = ctx.executor();
        let channel_ptr = ctx.channel() as *const dyn spark_core::pipeline::channel::Channel;
        let logger_ptr = ctx.logger() as *const dyn Logger;
        drop(binding);
        drop(decision);
        let dispatch = ServiceDispatchContext {
            executor,
            call_ctx,
            channel: unsafe { &*channel_ptr },
            logger: unsafe { &*logger_ptr },
            trace,
        };
        self.spawn_service_task(dispatch, service, request);
    }
}

impl InboundHandler for RouterHandler {
    fn describe(&self) -> MiddlewareDescriptor {
        self.descriptor.clone()
    }

    fn on_channel_active(&self, _ctx: &dyn Context) {}

    fn on_read(&self, ctx: &dyn Context, msg: PipelineMessage) {
        let snapshot = self.router.snapshot();
        let trace_clone = ctx.trace_context().clone();
        let parts = match self.context_builder.build(ctx, &msg, snapshot) {
            Ok(parts) => parts,
            Err(err) => {
                ctx.logger().error(
                    "router-handler failed to build routing context",
                    Some(&err),
                    Some(ctx.trace_context()),
                );
                return;
            }
        };

        let routing_ctx = RoutingContext::new(
            &msg,
            &parts.intent,
            parts.connection.as_ref(),
            Some(ctx.trace_context()),
            &parts.dynamic_metadata,
            snapshot,
        );

        match self.router.route_dyn(routing_ctx) {
            Ok(decision) => {
                if !decision.warnings().is_empty() {
                    let mut attributes = OwnedAttributeSet::new();
                    let joined = decision
                        .warnings()
                        .iter()
                        .map(|warn| warn.as_ref())
                        .collect::<Vec<_>>()
                        .join("; ");
                    attributes.push_owned("router.warnings", joined);
                    ctx.logger().warn_with_fields(
                        "router-handler received decision warnings",
                        attributes.as_slice(),
                        Some(ctx.trace_context()),
                    );
                }
                self.handle_decision(ctx, decision, msg, trace_clone);
            }
            Err(err) => {
                let spark_error = match err {
                    spark_core::router::traits::generic::RouteError::NotFound {
                        pattern, ..
                    } => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("route pattern `{pattern:?}` not found"),
                    ),
                    spark_core::router::traits::generic::RouteError::PolicyDenied { reason } => {
                        SparkError::new(
                            codes::APP_ROUTING_FAILED,
                            format!("route rejected by policy: {reason}"),
                        )
                    }
                    spark_core::router::traits::generic::RouteError::ServiceUnavailable {
                        id,
                        source,
                    } => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("service bound to route `{id:?}` is unavailable"),
                    )
                    .with_cause(source),
                    spark_core::router::traits::generic::RouteError::Internal(inner) => inner,
                    other => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("unexpected route error: {other:?}"),
                    ),
                };
                ctx.logger().error(
                    "router-handler failed to resolve route",
                    Some(&spark_error),
                    Some(ctx.trace_context()),
                );
            }
        }
    }

    fn on_read_complete(&self, _ctx: &dyn Context) {}

    fn on_writability_changed(&self, _ctx: &dyn Context, _is_writable: bool) {}

    fn on_user_event(&self, _ctx: &dyn Context, _event: spark_core::observability::CoreUserEvent) {}

    fn on_exception_caught(&self, _ctx: &dyn Context, _error: CoreError) {}

    fn on_channel_inactive(&self, _ctx: &dyn Context) {}
}
