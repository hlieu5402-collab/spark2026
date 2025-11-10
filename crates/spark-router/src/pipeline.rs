use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use core::any::TypeId;

use spark_core::{
    CoreError, SparkError,
    buffer::PipelineMessage,
    error::codes,
    observability::{Logger, OwnedAttributeSet},
    pipeline::{
        Context, Handler, HandlerDirection, InboundHandler, extensions::ExtensionsMap,
        initializer::InitializerDescriptor,
    },
    router::{
        DynRouter, RouteDecisionObject, RouteError,
        context::{RoutingContext, RoutingIntent, RoutingSnapshot},
        metadata::RouteMetadata,
    },
    runtime::TaskExecutor,
    service::BoxService,
    transport::intent::ConnectionIntent,
};

/// ApplicationRouter 模块内使用的扩展存储键类型，占位以提供稳定的 `TypeId`。
struct RouterContextSlot;

/// `RouterContextState` 聚合路由判定所需的上下文快照。
///
/// # 教案式说明
/// - **意图（Why）**：将跨组件收集的路由意图、连接属性与动态元数据集中管理，避免 Handler 彼此约定魔法字段。
/// - **定位（Where）**：存放于 `Channel` 的 [`ExtensionsMap`] 中，供 [`ApplicationRouter`] 在入站阶段读取。
/// - **逻辑（How）**：提供快照式只读访问接口，并在需要时克隆出独立副本，确保在异步路由判定期间不会悬垂引用。
/// - **契约（What）**：
///   - `intent`：请求方声明的路由意图，必须完整描述目标模式；
///   - `connection`：传输层意图（可选），用于 QoS、安全等协商；
///   - `dynamic_metadata`：运行时追加的标签集合，在路由决策时与静态元数据合并。
/// - **权衡（Trade-offs）**：结构体保持不可变，若需动态更新应在上游生成新的快照重新写入扩展存储，以换取读路径的无锁访问。
#[derive(Clone, Debug)]
pub struct RouterContextState {
    intent: RoutingIntent,
    connection: Option<ConnectionIntent>,
    dynamic_metadata: RouteMetadata,
}

impl RouterContextState {
    /// 基于必需的路由意图构造状态。
    ///
    /// # 教案式说明
    /// - **前置条件**：`intent` 必须指向合法的 [`RoutePattern`](spark_core::router::RoutePattern)，调用方应在上游完成语义校验；
    /// - **执行步骤**：初始化时动态元数据置空，留待后续组件补充；
    /// - **后置条件**：返回的状态可立即存入扩展映射，并被 [`ApplicationRouter`] 读取。
    pub fn new(intent: RoutingIntent) -> Self {
        Self {
            intent,
            connection: None,
            dynamic_metadata: RouteMetadata::new(),
        }
    }

    /// 附加传输层意图，用于在路由阶段融合 QoS/安全偏好。
    pub fn with_connection(mut self, connection: ConnectionIntent) -> Self {
        self.connection = Some(connection);
        self
    }

    /// 访问动态元数据的可变引用，便于在上游组件中填充运行时标签。
    pub fn dynamic_metadata_mut(&mut self) -> &mut RouteMetadata {
        &mut self.dynamic_metadata
    }

    /// 读取内部路由意图。
    pub fn intent(&self) -> &RoutingIntent {
        &self.intent
    }

    /// 读取可选的连接意图。
    pub fn connection(&self) -> Option<&ConnectionIntent> {
        self.connection.as_ref()
    }

    /// 读取动态元数据。
    pub fn dynamic_metadata(&self) -> &RouteMetadata {
        &self.dynamic_metadata
    }

    /// 克隆出一次性消费的快照，供 Handler 构建 [`RoutingContext`]。
    pub fn snapshot(&self) -> RouterContextSnapshot {
        RouterContextSnapshot {
            intent: self.intent.clone(),
            connection: self.connection.clone(),
            dynamic_metadata: self.dynamic_metadata.clone(),
        }
    }
}

/// 供 [`ApplicationRouter`] 使用的只读快照。
#[derive(Clone, Debug)]
pub struct RouterContextSnapshot {
    pub intent: RoutingIntent,
    pub connection: Option<ConnectionIntent>,
    pub dynamic_metadata: RouteMetadata,
}

/// 将路由上下文状态写入通道扩展存储。
///
/// # 教案式说明
/// - **意图（Why）**：提供统一入口，避免调用方直接操作 [`ExtensionsMap`] 时出现键冲突或类型漂移。
/// - **逻辑（How）**：以 `Arc` 包裹状态对象后写入，保证多 Handler 并发读取时的线程安全；
/// - **契约（What）**：后续可调用 [`load_router_context`] 获取同一份 `Arc`。若需更新，调用方应先移除再写入新值。
pub fn store_router_context(extensions: &dyn ExtensionsMap, state: RouterContextState) {
    extensions.insert(TypeId::of::<RouterContextSlot>(), Box::new(Arc::new(state)));
}

/// 从通道扩展存储中读取路由上下文状态。
pub fn load_router_context(extensions: &dyn ExtensionsMap) -> Option<Arc<RouterContextState>> {
    extensions
        .get(&TypeId::of::<RouterContextSlot>())
        .and_then(|entry| entry.downcast_ref::<Arc<RouterContextState>>())
        .map(Arc::clone)
}

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

/// 为 [`ApplicationRouter`] 提供请求上下文所需材料的构造器契约。
///
/// # 教案式说明
/// - **意图（Why）**：不同接入层在 [`PipelineMessage`] 中承载上下文的方式各异，
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

/// 默认的上下文构造器，实现从 [`ExtensionsMap`] 中提取 [`RouterContextState`]。
///
/// # 教案式说明
/// - **意图（Why）**：为沿用旧版“控制面先写入 `RouterContextState`，Handler 直接消费” 的调用模式提供兼容层，
///   降低本次路由模块合并的迁移成本。
/// - **逻辑（How）**：
///   1. 调用 [`load_router_context`] 读取共享状态；
///   2. 若缺失则返回 `SparkError`，提示调用方补充意图；
///   3. 对读取到的状态执行 `snapshot()`，生成 [`RoutingContextParts`]。
/// - **契约（What）**：要求在 Handler 触发前由上游组件通过 [`store_router_context`] 写入状态；否则将返回
///   `codes::APP_ROUTING_FAILED` 错误并终止处理。
#[derive(Clone, Debug, Default)]
pub struct ExtensionsRoutingContextBuilder;

impl RoutingContextBuilder for ExtensionsRoutingContextBuilder {
    fn build(
        &self,
        ctx: &dyn Context,
        _msg: &PipelineMessage,
        _snapshot: RoutingSnapshot<'_>,
    ) -> spark_core::Result<RoutingContextParts, SparkError> {
        let Some(state) = load_router_context(ctx.channel().extensions()) else {
            return Err(SparkError::new(
                codes::APP_ROUTING_FAILED,
                "router context missing on channel",
            ));
        };
        let snapshot = state.snapshot();
        Ok(RoutingContextParts::new(
            snapshot.intent,
            snapshot.connection,
            snapshot.dynamic_metadata,
        ))
    }
}

/// 封装服务调用所需的调度句柄集合。
///
/// # 教案式说明
/// - **意图（Why）**：将执行器、调用上下文、写通道与日志器等多项依赖打包，
///   既便于 [`ApplicationRouter::spawn_service_task`] 复用，也让参数列表控制在 Clippy 建议范围内。
/// - **逻辑（How）**：在 Handler 中就地构造 `ServiceDispatchContext`，
///   持有 [`spark_core::contract::CallContext`] 所有权，并克隆通道 `Arc` 与追踪上下文；随后交由异步任务消费。
/// - **契约（What）**：
///   - `executor`：运行时执行器引用，必须保证在任务生命周期内有效；
///   - `call_ctx`：任务所属的调用上下文，要求克隆自当前 Pipeline；
///   - `channel`：写响应所需的通道引用，由 [`Arc`] 管理生命周期；
///   - `logger`：用于记录任务执行失败的日志器指针；
///   - `trace`：追踪上下文，用于日志注入；
/// - **风险提示（Trade-offs）**：`logger` 通过 `unsafe` 指针延长生命周期，假设 `HotSwapContext` 以 `Arc`
///   持有底层实现；若未来上下文模型变化，需要同步调整此结构以维持内存安全。
struct ServiceDispatchContext<'exec> {
    executor: &'exec dyn TaskExecutor,
    call_ctx: spark_core::contract::CallContext,
    channel: Arc<dyn spark_core::pipeline::channel::Channel>,
    logger: &'static dyn Logger,
    trace: spark_core::observability::TraceContext,
}

/// `ApplicationRouter` 负责终止入站事件并将请求转交给对象层 Router。
///
/// # 教案式说明
/// - **意图（Why）**：将 Pipeline 最终的业务请求映射到 [`DynRouter`]，实现“Handler → Router → Service” 的桥接，
///   使得 Handler 链可以专注于编解码、鉴权等前置步骤。
/// - **逻辑（How）**：
///   1. 通过 [`RoutingContextBuilder`] 获取构建 [`RoutingContext`] 所需的材料；
///   2. 调用注入的 [`DynRouter::route_dyn`] 执行路由决策；
///   3. 将命中的 [`BoxService`] 托付给运行时执行器，异步调用业务逻辑；
///   4. 调用成功后写回响应，触发出站链路。
/// - **契约（What）**：
///   - Handler 不再调用 `ctx.forward_read`，意味着它必须位于入站链路尾部；
///   - 路由或服务调用失败会记录 ERROR 日志，但不会重复抛给后续 Handler。
/// - **风险（Trade-offs）**：
///   - 通过 `unsafe` 延长 `Logger` 引用生命周期以便在异步任务中使用，
///     假定 `HotSwapContext` 内部使用 `Arc` 持有该资源；若未来上下文实现发生变化，需同步评估安全性。
pub struct ApplicationRouter {
    router: Arc<dyn DynRouter>,
    context_builder: Arc<dyn RoutingContextBuilder>,
    descriptor: InitializerDescriptor,
}

impl Clone for ApplicationRouter {
    fn clone(&self) -> Self {
        Self {
            router: Arc::clone(&self.router),
            context_builder: Arc::clone(&self.context_builder),
            descriptor: self.descriptor.clone(),
        }
    }
}

impl ApplicationRouter {
    /// 构造新的路由处理器实例。
    ///
    /// # 参数说明
    /// - `router`：对象层路由器实现，负责根据 [`RoutingContext`] 返回服务绑定；
    /// - `context_builder`：提取路由上下文材料的扩展点；
    /// - `descriptor`：用于链路 introspection 的元信息，如无特殊需求可传入 [`InitializerDescriptor::anonymous`] 结果。
    pub fn new(
        router: Arc<dyn DynRouter>,
        context_builder: Arc<dyn RoutingContextBuilder>,
        descriptor: InitializerDescriptor,
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
        let channel = Arc::clone(&dispatch.channel);
        let logger = dispatch.logger;
        let trace_for_log = dispatch.trace.clone();
        let service_task = async move {
            let mut dyn_service = into_dyn_service(service);
            let response = dyn_service.call_dyn(fut_ctx, request).await?;
            channel.write(response).map(|_| ()).map_err(|err| {
                SparkError::new(codes::APP_ROUTING_FAILED, "failed to write response")
                    .with_cause(err)
            })
        };

        let log_future = async move {
            if let Err(err) = service_task.await {
                logger.error(
                    "application-router encountered error while invoking service",
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
        let channel = unsafe_clone_channel(ctx);
        let logger_ptr = ctx.logger() as *const dyn Logger;
        drop(binding);
        drop(decision);
        let dispatch = ServiceDispatchContext {
            executor,
            call_ctx,
            channel,
            logger: unsafe { &*logger_ptr },
            trace,
        };
        self.spawn_service_task(dispatch, service, request);
    }

    /// 将当前 Handler 实例转化为 `Arc<dyn Handler>`，方便 PipelineInitializer 直接注册。
    ///
    /// # 教案式说明
    /// - **意图（Why）**：减少调用方在装配链路时的样板代码，直接得到框架契约期望的对象类型；
    /// - **契约（What）**：返回的 `Arc` 既实现 [`Handler`] 又能通过 [`Handler::clone_inbound`] 克隆入站实例；
    /// - **前置条件（Contract）**：当前结构体持有的 Router、上下文构造器均已满足 `Arc` 克隆语义；
    /// - **后置条件（Contract）**：调用方可将返回值直接交给 [`Pipeline::add_handler_after`](spark_core::pipeline::Pipeline::add_handler_after)。
    pub fn into_handler(self) -> Arc<dyn Handler> {
        Arc::new(self)
    }
}

impl Handler for ApplicationRouter {
    fn direction(&self) -> HandlerDirection {
        HandlerDirection::Inbound
    }

    fn descriptor(&self) -> InitializerDescriptor {
        self.descriptor.clone()
    }

    fn clone_inbound(&self) -> Option<Arc<dyn InboundHandler>> {
        let handler: Arc<ApplicationRouter> = Arc::new(self.clone());
        Some(handler)
    }
}

impl InboundHandler for ApplicationRouter {
    fn describe(&self) -> InitializerDescriptor {
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
                    "application-router failed to build routing context",
                    Some(&err),
                    Some(ctx.trace_context()),
                );
                return;
            }
        };

        // --- 请求元数据审查逻辑 --------------------------------------------------------
        // 教案式说明：
        // 1. **意图 (Why)**：业务路由往往依赖帧头/动态标签等元数据。若消息缺乏这类字段，路由器
        //    可能只得依赖默认路由或直接拒绝，为了在热路径中即时发现这类异常，我们在 Handler 内
        //    主动检测并输出诊断日志。
        // 2. **逻辑 (How)**：
        //    - 调用 [`PipelineMessage::user_kind`] 获取业务帧类型；
        //    - 读取 `RoutingContextParts::dynamic_metadata` 判断是否已填充标签；
        //    - 若两者任一缺失，则输出 DEBUG 日志，帮助排查上游编解码/上下文构造器是否遗漏。
        // 3. **契约 (What)**：
        //    - 仅在 `message_kind` 或 `metadata` 缺失时记录日志，避免为健康流量增加噪音；
        //    - 日志附带 `router.metadata_present`（布尔）与 `router.message_kind`（字符串）两个观测字段。
        // 4. **权衡 (Trade-offs)**：
        //    - 选择 DEBUG 级别以降低噪音，但依然为排查“路由意图缺失”问题提供第一手线索；
        //    - 诊断逻辑只读数据，不会在消息路径上复制缓冲或修改上下文，确保性能损耗可忽略。
        let message_kind = msg.user_kind();
        let metadata_empty = parts.dynamic_metadata.iter().next().is_none();
        if message_kind.is_none() || metadata_empty {
            let mut attributes = OwnedAttributeSet::new();
            attributes.push_owned(
                "router.metadata_present",
                !metadata_empty,
            );
            attributes.push_owned(
                "router.message_kind",
                message_kind.unwrap_or("<unknown>").to_owned(),
            );
            ctx.logger().debug_with_fields(
                "application-router inspected inbound message metadata",
                attributes.as_slice(),
                Some(ctx.trace_context()),
            );
        }

        let routing_ctx = RoutingContext::new(
            ctx.execution_context(),
            &msg,
            &parts.intent,
            parts.connection.as_ref(),
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
                        "application-router received decision warnings",
                        attributes.as_slice(),
                        Some(ctx.trace_context()),
                    );
                }
                self.handle_decision(ctx, decision, msg, trace_clone);
            }
            Err(err) => {
                let spark_error = match err {
                    RouteError::NotFound { pattern, .. } => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("route pattern `{pattern:?}` not found"),
                    ),
                    RouteError::PolicyDenied { reason } => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("route rejected by policy: {reason}"),
                    ),
                    RouteError::ServiceUnavailable { id, source } => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("service bound to route `{id:?}` is unavailable"),
                    )
                    .with_cause(source),
                    RouteError::Internal(inner) => inner,
                    other => SparkError::new(
                        codes::APP_ROUTING_FAILED,
                        format!("unexpected route error: {other:?}"),
                    ),
                };
                ctx.logger().error(
                    "application-router failed to resolve route",
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

/// 将 [`Context`] 暂借的通道引用转换为拥有型 [`Arc`]。
///
/// # 教案式说明
/// - **意图（Why）**：`ApplicationRouter` 在异步任务中需要写回响应，必须延长 [`Channel`](spark_core::pipeline::channel::Channel)
///   的生命周期；
/// - **逻辑（How）**：利用 `HotSwapContext` 内部以 [`Arc`] 保存通道的事实，先手动增加强引用计数，再通过
///   [`Arc::from_raw`] 恢复拥有权，确保未来在任务结束时安全释放；
/// - **契约（What）**：调用者需保证传入的 [`Context`] 实现确实由 [`Arc`] 创建通道引用（当前 `HotSwapContext`
///   满足该条件）；若未来更换实现，应同步调整此函数避免破坏内存安全。
fn unsafe_clone_channel(ctx: &dyn Context) -> Arc<dyn spark_core::pipeline::channel::Channel> {
    let channel_ref = ctx.channel();
    let ptr = channel_ref as *const dyn spark_core::pipeline::channel::Channel;
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// 将 [`BoxService`] 拆箱为可变的对象层服务引用。
///
/// # 教案式说明
/// - **意图（Why）**：[`spark_core::service::DynService`] 的调用接口要求可变引用，需消除外层 [`Arc`] 封装以便执行一次性的业务调用；
/// - **逻辑（How）**：利用“路由器每次返回独占实例”的前提，直接通过 [`Arc::into_raw`] 获取裸指针，再恢复为 `Box` 管理；
/// - **契约（What）**：调用前必须确保没有额外的句柄克隆，否则将导致双重释放；本实现依赖路由器工厂按需创建服务满足该假设。
fn into_dyn_service(service: BoxService) -> Box<dyn spark_core::service::DynService> {
    let arc = service.into_arc();
    debug_assert_eq!(
        Arc::strong_count(&arc),
        1,
        "router factory must yield unique services",
    );
    let raw = Arc::into_raw(arc) as *mut dyn spark_core::service::DynService;
    unsafe { Box::from_raw(raw) }
}
