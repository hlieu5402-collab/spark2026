use alloc::{boxed::Box, format, sync::Arc};
use core::any::TypeId;

use spark_core::{
    buffer::PipelineMessage,
    pipeline::{
        context::Context, extensions::ExtensionsMap, handler::InboundHandler,
        middleware::MiddlewareDescriptor,
    },
    router::{
        RoutingContext, RoutingIntent,
        metadata::RouteMetadata,
        traits::object::{DynRouter, RouteDecisionObject},
    },
    service::BoxService,
    transport::intent::ConnectionIntent,
};

/// RouterHandler 模块内使用的扩展存储键类型，占位以提供稳定的 `TypeId`。
struct RouterContextSlot;

/// `RouterContextState` 聚合路由判定所需的上下文快照。
///
/// # 教案式说明
/// - **意图（Why）**：将跨组件收集的路由意图、连接属性与动态元数据集中管理，避免 Handler 彼此约定魔法字段。
/// - **定位（Where）**：存放于 `Channel` 的 [`ExtensionsMap`] 中，供 `RouterHandler` 在入站阶段读取。
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
    /// - **前置条件**：`intent` 必须指向合法的 `RoutePattern`，调用方应在上游完成语义校验；
    /// - **执行步骤**：初始化时动态元数据置空，留待后续组件补充；
    /// - **后置条件**：返回的状态可立即存入扩展映射，并被 `RouterHandler` 读取。
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

/// 供 `RouterHandler` 使用的只读快照。
#[derive(Clone, Debug)]
pub struct RouterContextSnapshot {
    pub intent: RoutingIntent,
    pub connection: Option<ConnectionIntent>,
    pub dynamic_metadata: RouteMetadata,
}

/// 将路由上下文状态写入通道扩展存储。
///
/// # 教案式说明
/// - **意图（Why）**：提供统一入口，避免调用方直接操作 `ExtensionsMap` 时出现键冲突或类型漂移。
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

/// `RouterHandler` 将 Pipeline 入站事件桥接到对象层路由器。
///
/// # 教案式说明
/// - **意图（Why）**：终止默认的入站传播链路，并将请求交由 `DynRouter` 做目标选路与服务实例创建，实现“Pipeline → Router → Service”的解耦。
/// - **定位（Where）**：通常作为入站链路的末端 Handler，紧随编解码器或鉴权模块。
/// - **逻辑（How）**：
///   1. 从 `Channel` 扩展中读取 [`RouterContextState`] 构造 `RoutingContext`；
///   2. 调用注入的 `DynRouter` 获取 [`RouteDecisionObject`]；
///   3. 将对象层 [`BoxService`] 拆箱并异步执行；
///   4. 任务完成后，通过克隆的 `Channel` 句柄写回响应，触发出站链路。
/// - **契约（What）**：
///   - Handler 不再调用 `ctx.forward_read`，因此必须保证自身负责处理完请求或显式记录失败；
///   - 异步任务使用 `ctx.executor()` 派发，并复用原始的 [`CallContext`](spark_core::contract::CallContext)。
/// - **权衡（Trade-offs）**：通过克隆 `Arc<dyn Channel>` 在后台任务中写回，换取安全的上下文传播；若未来运行时支持显式的出站调度，可进一步优化为事件式回写。
pub struct RouterHandler {
    router: Arc<dyn DynRouter>,
    descriptor: MiddlewareDescriptor,
}

impl RouterHandler {
    /// 以对象层路由器构造 Handler。
    ///
    /// # 教案式说明
    /// - **输入**：`router` 为线程安全、可热更新的 `DynRouter` 实例；
    /// - **行为**：记录路由器引用并准备描述符，供观测系统识别 Handler；
    /// - **后置条件**：返回的 Handler 可直接注册到 Pipeline 控制器。
    pub fn new(router: Arc<dyn DynRouter>) -> Self {
        let descriptor = MiddlewareDescriptor::anonymous("router-handler");
        Self { router, descriptor }
    }

    fn describe_router_decision(&self, ctx: &dyn Context, decision: &RouteDecisionObject) {
        if decision.warnings().is_empty() {
            return;
        }
        let logger = ctx.logger();
        let trace = Some(ctx.trace_context());
        for warning in decision.warnings() {
            logger.warn(warning.as_ref(), trace);
        }
    }
}

impl InboundHandler for RouterHandler {
    fn describe(&self) -> MiddlewareDescriptor {
        self.descriptor.clone()
    }

    fn on_channel_active(&self, _ctx: &dyn Context) {}

    fn on_read(&self, ctx: &dyn Context, msg: PipelineMessage) {
        let Some(state) = load_router_context(ctx.channel().extensions()) else {
            ctx.logger().error(
                "router context missing on channel",
                None,
                Some(ctx.trace_context()),
            );
            return;
        };
        let snapshot = state.snapshot();
        let routing_snapshot = self.router.snapshot();
        let request_ref = &msg;
        let connection_ref = snapshot.connection.as_ref();
        let routing_context = RoutingContext::new(
            request_ref,
            &snapshot.intent,
            connection_ref,
            Some(ctx.trace_context()),
            &snapshot.dynamic_metadata,
            routing_snapshot,
        );

        let decision = match self.router.route_dyn(routing_context) {
            Ok(decision) => decision,
            Err(error) => {
                ctx.logger().error(
                    &format!("router rejected request: {error:?}"),
                    None,
                    Some(ctx.trace_context()),
                );
                return;
            }
        };

        self.describe_router_decision(ctx, &decision);
        let service = decision.binding().service().clone();

        let call_ctx = ctx.call_context().clone();
        let channel = unsafe_clone_channel(ctx);
        let executor = ctx.executor();
        let future = async move {
            drive_service_call(service, call_ctx, msg, channel).await;
            Ok::<Box<dyn core::any::Any + Send>, spark_core::TaskError>(Box::new(()))
        };
        let join = executor.spawn_dyn(ctx.call_context(), Box::pin(future));
        join.detach();
    }

    fn on_read_complete(&self, _ctx: &dyn Context) {}

    fn on_writability_changed(&self, _ctx: &dyn Context, _is_writable: bool) {}

    fn on_user_event(&self, _ctx: &dyn Context, _event: spark_core::observability::CoreUserEvent) {}

    fn on_exception_caught(&self, _ctx: &dyn Context, _error: spark_core::CoreError) {}

    fn on_channel_inactive(&self, _ctx: &dyn Context) {}
}

/// 将 [`Context`] 暂借的通道引用转换为拥有型 `Arc`。
///
/// # 教案式说明
/// - **意图（Why）**：`RouterHandler` 在异步任务中需要写回响应，必须延长 `Channel` 的生命周期；
/// - **逻辑（How）**：利用 `HotSwapContext` 内部以 `Arc` 保存通道的事实，先手动增加强引用计数，再通过
///   [`Arc::from_raw`] 恢复拥有权，确保未来在任务结束时安全释放；
/// - **契约（What）**：调用者需保证传入的 `Context` 实现确实由 `Arc` 创建通道引用（当前 `HotSwapContext`
///   满足该条件）；若未来更换实现，应同步调整此函数避免破坏内存安全。
fn unsafe_clone_channel(ctx: &dyn Context) -> Arc<dyn spark_core::pipeline::channel::Channel> {
    let channel_ref = ctx.channel();
    let ptr = channel_ref as *const dyn spark_core::pipeline::channel::Channel;
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
}

/// 在后台任务中驱动对象层 Service 并写回响应。
///
/// # 教案式说明
/// - **意图（Why）**：异步执行对象层服务以避免阻塞 Pipeline 事件循环，同时复用统一的写路径将响应送回客户端。
/// - **逻辑（How）**：
///   1. 将 [`BoxService`] 拆解为可变的 [`DynService`]；
///   2. 调用 `call_dyn` 完成业务处理；
///   3. 成功时通过克隆的 `Channel` 写回消息，若写入失败则静默丢弃并交由底层背压处理；
/// - **契约（What）**：
///   - 任务持有原始 [`CallContext`]，确保取消/截止语义继续生效；
///   - `Channel` 必须在任务生命周期内保持有效，由 `Arc` 管理其引用计数。
async fn drive_service_call(
    service: BoxService,
    call_ctx: spark_core::contract::CallContext,
    req: PipelineMessage,
    channel: Arc<dyn spark_core::pipeline::channel::Channel>,
) {
    let mut service = into_dyn_service(service);
    let result = service.call_dyn(call_ctx, req).await;
    match result {
        Ok(response) => {
            if channel.write(response).is_ok() {
                channel.flush();
            }
        }
        Err(_error) => {
            // TODO(#router-handler-error-observability): 当前缺乏可克隆的 Logger 以在异步
            //  任务内记录错误，只能依赖上游指标监控；未来可在 `CallContext` 中注入
            //  可复用的观测句柄，避免静默失败。
        }
    }
}

/// 将 `BoxService` 拆箱为可变的对象层服务引用。
///
/// # 教案式说明
/// - **意图（Why）**：`DynService` 的调用接口要求可变引用，需消除外层 `Arc` 封装以便执行一次性的业务调用；
/// - **逻辑（How）**：利用“路由器每次返回独占实例”的前提，直接通过 [`Arc::into_raw`] 获取裸指针，再恢复为 `Box` 管理；
/// - **契约（What）**：调用前必须确保没有额外的句柄克隆，否则将导致双重释放；本实现依赖路由器工厂按需创建服务满足该假设。
fn into_dyn_service(service: BoxService) -> Box<dyn spark_core::service::DynService> {
    let arc = service.into_arc();
    debug_assert_eq!(
        Arc::strong_count(&arc),
        1,
        "router factory must yield unique services"
    );
    let raw = Arc::into_raw(arc) as *mut dyn spark_core::service::DynService;
    unsafe { Box::from_raw(raw) }
}
