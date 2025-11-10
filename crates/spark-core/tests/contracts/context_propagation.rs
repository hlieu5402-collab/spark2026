use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use spark_core::buffer::{
    BufferAllocator, ErasedSparkBuf, ErasedSparkBufMut, PipelineMessage, ReadableBuffer,
    WritableBuffer,
};
use spark_core::codec::decoder::{DecodeContext, DecodeOutcome};
use spark_core::codec::encoder::{EncodeContext, EncodedPayload};
use spark_core::codec::metadata::{CodecDescriptor, ContentEncoding, ContentType};
use spark_core::codec::Codec;
use spark_core::context::Context;
use spark_core::contract::{
    Budget, BudgetKind, CallContext, Cancellation, Deadline, SecurityContextSnapshot,
    DEFAULT_OBSERVABILITY_CONTRACT,
};
use spark_core::error::codes;
use spark_core::future::BoxFuture;
use spark_core::observability::trace::{TraceContext, TraceFlags};
use spark_core::pipeline::controller::PipelineHandleId;
use spark_core::pipeline::{Channel, ChannelState, HandlerRegistry, Pipeline, WriteSignal};
use spark_core::router::binding::{RouteBinding, RouteDecision};
use spark_core::router::catalog::{RouteCatalog, RouteDescriptor};
use spark_core::router::context::{RoutingContext, RoutingIntent, RoutingSnapshot};
use spark_core::router::metadata::{MetadataKey, MetadataValue, RouteMetadata};
use spark_core::router::route::{RouteId, RouteKind, RoutePattern, RouteSegment};
use spark_core::router::{RouteError, Router};
use spark_core::security::IdentityDescriptor;
use spark_core::service::Service;
use spark_core::status::{ReadyCheck, ReadyState};
use spark_core::transport::factory::ListenerConfig;
use spark_core::transport::intent::{
    ConnectionIntent, QualityOfService, SecurityMode, SessionLifecycle,
};
use spark_core::transport::server::ListenerShutdown;
use spark_core::transport::traits::generic::TransportFactory;
use spark_core::transport::ServerChannel;
use spark_core::transport::{
    Endpoint, HandshakeOffer, PipelineInitializerSelector, TransportSocketAddr, Version, negotiate,
};
use spark_core::{CoreError, SparkError};

use super::support::{build_identity, monotonic, Lcg64};

/// 端到端上下文传递契约测试。
///
/// # 设计目标（Why）
/// - 验证 `CallContext` 在服务、路由、编解码、传输之间传播时不会丢失取消/截止/预算语义；
/// - 通过可重放的 deterministic 调度，确认取消信号在任意阶段触发后都会被后续组件观察到，避免“取消被超时饿死”；
/// - 为 C2 契约提供可回归的基线，确保未来重构或适配对象层时能够快速发现破坏性改动。
///
/// # 实施策略（How）
/// - 构建可记录上下文快照的伪实现（Service/Router/Codec/Transport），每次访问上下文都会采样关键字段；
/// - 使用 `Lcg64` 控制取消触发点、路由元数据大小等变量，生成 deterministic 事件轨迹；
/// - 将首轮执行的轨迹作为基线，对后续 100 次重放进行逐一比对；
/// - 在取消触发后断言所有后续上下文观测值均已标记 `cancelled = true`，证明取消优先级高于超时。
///
/// # 验收要点（What）
/// - **重放一致性**：同一 seed 下 100 次执行的事件序列完全相同；
/// - **取消优先级**：取消事件记录为 true，且事件之后所有带取消字段的快照均为 true；
/// - **上下文完整性**：服务、编解码、传输阶段的快照保留原始截止时间与 Flow 预算上限，路由阶段保留 QoS、安全意图与 trace 采样标记。
#[test]
fn context_propagation_contract_is_deterministic() {
    const SEED: u64 = 0xC2C0_N73X;
    const REPLAY_TIMES: usize = 100;

    let baseline = execute_replay(SEED);
    assert!(
        baseline
            .iter()
            .any(|event| matches!(event.stage, Stage::CancellationInjected(_))),
        "至少需要一次取消注入以验证优先级契约"
    );
    assert!(
        baseline.iter().any(|event| {
            matches!(event.stage, Stage::ServicePollReady | Stage::ServiceCall)
                && event.identity.is_some()
        }),
        "服务阶段必须观察到调用身份"
    );
    assert!(
        baseline
            .iter()
            .any(|event| matches!(event.stage, Stage::ServicePollReady)
                && event.peer_identity.is_some()),
        "服务阶段必须观察到对端身份"
    );
    assert!(
        baseline.iter().any(|event| event.trace_sampled.is_some()),
        "上下文应记录追踪采样标记"
    );

    let mut saw_cancel = false;
    for event in &baseline {
        match event.stage {
            Stage::CancellationInjected(_) => {
                saw_cancel = true;
                assert_eq!(event.cancelled, Some(true), "取消事件必须成功触发");
            }
            _ if saw_cancel => {
                if let Some(cancelled) = event.cancelled {
                    assert!(cancelled, "取消触发后所有后续上下文都必须观察到取消状态");
                }
            }
            _ => {}
        }
    }

    for iteration in 0..REPLAY_TIMES {
        let replay = execute_replay(SEED);
        assert_eq!(
            replay, baseline,
            "第 {iteration} 次重放的上下文轨迹发生漂移"
        );
    }
}

/// 使用确定性随机序列驱动一次上下文传播流程，并返回阶段快照。
///
/// # 设计初衷（Why）
/// - 将服务、路由、编解码、传输四个维度串联起来，确保相同 seed 下的行为完全可重放；
/// - 在可控点注入取消信号，验证之后的上下文均能观察到该状态，证明取消优先级最高；
/// - 记录阶段快照，便于测试在未来分析差异来源。
///
/// # 逻辑步骤（How）
/// 1. 初始化 `CallContext` 并构造伪实现组件；
/// 2. 根据 `seed` 选择取消触发阶段、路由元数据规模、QoS 与安全模式；
/// 3. 依次执行路由决策、服务就绪检查与调用、编解码、传输建连/绑定；
/// 4. 每步采集上下文快照并存储于共享事件表中；
/// 5. 返回事件表拷贝作为执行轨迹。
///
/// # 输入/输出契约（What）
/// - **参数**：`seed` 控制所有伪随机决策；
/// - **返回值**：阶段快照列表，按执行顺序排列；
/// - **前置条件**：无；
/// - **后置条件**：列表至少包含一次取消注入、一次服务调用、一次编解码与一次传输操作快照。
fn execute_replay(seed: u64) -> Vec<StageObservation> {
    const TOTAL_STAGES: usize = 7;

    let events: Arc<Mutex<Vec<StageObservation>>> = Arc::new(Mutex::new(Vec::new()));
    let mut rng = Lcg64::new(seed);

    let cancel_stage = rng.next_in_range(TOTAL_STAGES as u64) as usize;
    let metadata_items = (rng.next_in_range(3) + 1) as usize;
    let qos_choice = match rng.next_in_range(3) {
        0 => QualityOfService::Interactive,
        1 => QualityOfService::Streaming,
        _ => QualityOfService::BulkTransfer,
    };
    let security_choice = if rng.next_in_range(2) == 0 {
        SecurityMode::EnforcedTls
    } else {
        SecurityMode::Plaintext
    };
    let trace_is_sampled = rng.next_in_range(2) == 0;

    let mut cancellation = Cancellation::new();
    let base_time = monotonic(2, 0);
    let deadline = Deadline::with_timeout(base_time, Duration::from_millis(40));
    let flow_budget = Budget::new(BudgetKind::Flow, 128);

    let trace_flags = if trace_is_sampled {
        TraceFlags::SAMPLED
    } else {
        0
    };
    let trace_context = TraceContext::new(
        [0xAB; TraceContext::TRACE_ID_LENGTH],
        [0xCD; TraceContext::SPAN_ID_LENGTH],
        TraceFlags::new(trace_flags),
    );

    let security = SecurityContextSnapshot::default()
        .with_identity(build_identity("orders.caller"))
        .with_peer_identity(build_identity("orders.peer"));

    let call_context = CallContext::builder()
        .with_cancellation(cancellation.clone())
        .with_deadline(deadline)
        .add_budget(flow_budget.clone())
        .with_security(security)
        .with_observability(DEFAULT_OBSERVABILITY_CONTRACT)
        .with_trace_context(trace_context.clone())
        .build();

    let route_pattern = RoutePattern::new(
        RouteKind::Rpc,
        vec![
            RouteSegment::Literal(Cow::Borrowed("orders")),
            RouteSegment::Literal(Cow::Borrowed("create")),
        ],
    );
    let route_id = RouteId::new(
        RouteKind::Rpc,
        vec![Cow::Borrowed("orders"), Cow::Borrowed("create")],
    );

    let mut catalog = RouteCatalog::new();
    catalog.push(RouteDescriptor::new(route_pattern.clone()));

    let mut metadata = RouteMetadata::new();
    for index in 0..metadata_items {
        metadata.insert(
            MetadataKey::new(Cow::Owned(format!("tenant.meta.{index}"))),
            MetadataValue::Integer(i64::try_from(index).unwrap()),
        );
    }
    let dynamic_metadata = metadata.clone();

    let mut intent = RoutingIntent::new(route_pattern.clone()).with_metadata(metadata);
    intent = intent.with_expected_qos(qos_choice);
    intent = intent.with_security_preference(security_choice.clone());

    let endpoint = Endpoint::logical("spark".to_string(), "orders".to_string());
    let connection_intent = ConnectionIntent::new(endpoint.clone())
        .with_qos(qos_choice)
        .with_security(security_choice.clone())
        .with_lifecycle(SessionLifecycle::BidirectionalStream);

    let router_request = PipelineMessage::from_user(TestUserMessage { tag: rng.next() });
    let routing_snapshot = RoutingSnapshot::new(&catalog, 7);
    let routing_context = RoutingContext::new(
        call_context.execution(),
        &router_request,
        &intent,
        Some(&connection_intent),
        &dynamic_metadata,
        routing_snapshot,
    );

    let service = RecordingServiceHandle::new(events.clone(), flow_budget.clone());
    let router = RecordingRouter::new(events.clone(), service.clone(), route_id, catalog);
    let codec = RecordingCodec::new(events.clone());
    let transport_addr = TransportSocketAddr::V4 {
        addr: [127, 0, 0, 1],
        port: 2443,
    };
    let transport_factory = RecordingTransportFactory::new(events.clone(), transport_addr);

    let listener_config = ListenerConfig::new(endpoint.clone());
    let controller_factory = Arc::new(DummyPipelineFactory);
    let allocator = NoopAllocator;

    let waker = noop_waker();
    let mut task_cx = TaskContext::from_waker(&waker);

    let mut stage_index = 0usize;
    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeRouterRoute,
    );
    router.route(routing_context).expect("路由决策应成功");
    stage_index += 1;

    let mut service_instance = service.clone();
    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeServicePollReady,
    );
    let poll_ready = service_instance.poll_ready(&call_context.execution(), &mut task_cx);
    assert!(matches!(
        poll_ready,
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    ));
    stage_index += 1;

    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeServiceCall,
    );
    let request_message = PipelineMessage::from_user(TestUserMessage { tag: rng.next() });
    let mut response_future = service_instance.call(call_context.clone(), request_message);
    let response = match Future::poll(Pin::new(&mut response_future), &mut task_cx) {
        Poll::Ready(Ok(msg)) => msg,
        Poll::Ready(Err(err)) => panic!("服务调用失败: {err:?}"),
        Poll::Pending => panic!("服务调用应立即完成"),
    };
    stage_index += 1;

    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeCodecEncode,
    );
    let mut encode_ctx =
        EncodeContext::with_limits(&allocator, Some(&flow_budget), Some(256), None);
    let payload = codec
        .encode(&response, &mut encode_ctx)
        .expect("编码不应失败");
    let mut inbound_buffer: Box<dyn ReadableBuffer> = payload.into_buffer();
    let mut decode_ctx =
        DecodeContext::with_limits(&allocator, Some(&flow_budget), Some(256), None);
    codec
        .decode(&mut *inbound_buffer, &mut decode_ctx)
        .expect("解码不应失败");
    stage_index += 1;

    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeTransportBind,
    );
    let scheme = transport_factory.scheme(&call_context.execution());
    assert_eq!(scheme, "deterministic-transport");
    let mut bind_future = transport_factory.bind(
        &call_context.execution(),
        listener_config.clone(),
        controller_factory.clone(),
    );
    let server = match Future::poll(Pin::new(&mut bind_future), &mut task_cx) {
        Poll::Ready(Ok(server)) => server,
        Poll::Ready(Err(err)) => panic!("传输绑定失败: {err:?}"),
        Poll::Pending => panic!("绑定流程应立即完成"),
    };
    stage_index += 1;

    maybe_trigger_cancel(
        &events,
        stage_index,
        cancel_stage,
        &mut cancellation,
        CancelPoint::BeforeTransportConnect,
    );
    let mut connect_future =
        transport_factory.connect(&call_context.execution(), connection_intent.clone(), None);
    let _channel = match Future::poll(Pin::new(&mut connect_future), &mut task_cx) {
        Poll::Ready(Ok(channel)) => channel,
        Poll::Ready(Err(err)) => panic!("传输建连失败: {err:?}"),
        Poll::Pending => panic!("建连流程应立即完成"),
    };
    server.local_addr(&call_context.execution());

    let snapshot = events.lock().expect("事件锁应可用").clone();
    snapshot
}

/// 记录阶段化上下文观测结果，便于比较重放轨迹。
#[derive(Clone, Debug, PartialEq, Eq)]
struct StageObservation {
    stage: Stage,
    cancelled: Option<bool>,
    deadline_ms: Option<u128>,
    flow_limit: Option<u64>,
    flow_remaining: Option<u64>,
    trace_sampled: Option<bool>,
    metadata_entries: Option<usize>,
    qos: Option<QualityOfService>,
    security_mode: Option<SecurityMode>,
    identity: Option<IdentityDescriptor>,
    peer_identity: Option<IdentityDescriptor>,
}

impl StageObservation {
    fn from_execution(stage: Stage, ctx: &Context<'_>) -> Self {
        let deadline_ms = ctx
            .deadline()
            .instant()
            .map(|instant| instant.as_duration().as_millis());
        let flow_budget = ctx.budget(&BudgetKind::Flow);
        Self {
            stage,
            cancelled: Some(ctx.cancellation().is_cancelled()),
            deadline_ms,
            flow_limit: flow_budget.map(|budget| budget.limit()),
            flow_remaining: flow_budget.map(|budget| budget.remaining()),
            trace_sampled: Some(ctx.trace_context().is_sampled()),
            metadata_entries: None,
            qos: None,
            security_mode: None,
            identity: ctx.identity().cloned(),
            peer_identity: ctx.peer_identity().cloned(),
        }
    }

    fn from_call(stage: Stage, ctx: &CallContext) -> Self {
        let flow_budget = ctx.budget(&BudgetKind::Flow);
        Self {
            stage,
            cancelled: Some(ctx.cancellation().is_cancelled()),
            deadline_ms: ctx
                .deadline()
                .instant()
                .map(|instant| instant.as_duration().as_millis()),
            flow_limit: flow_budget.as_ref().map(|budget| budget.limit()),
            flow_remaining: flow_budget.as_ref().map(|budget| budget.remaining()),
            trace_sampled: Some(ctx.trace_context().is_sampled()),
            metadata_entries: None,
            qos: None,
            security_mode: None,
            identity: ctx
                .security()
                .identity()
                .map(|identity| identity.as_ref().clone()),
            peer_identity: ctx
                .security()
                .peer_identity()
                .map(|identity| identity.as_ref().clone()),
        }
    }

    fn from_encode(stage: Stage, ctx: &EncodeContext<'_>) -> Self {
        let flow_budget = ctx.budget();
        Self {
            stage,
            cancelled: None,
            deadline_ms: None,
            flow_limit: flow_budget.map(|budget| budget.limit()),
            flow_remaining: flow_budget.map(|budget| budget.remaining()),
            trace_sampled: None,
            metadata_entries: None,
            qos: None,
            security_mode: None,
            identity: None,
            peer_identity: None,
        }
    }

    fn from_decode(stage: Stage, ctx: &DecodeContext<'_>) -> Self {
        let flow_budget = ctx.budget();
        Self {
            stage,
            cancelled: None,
            deadline_ms: None,
            flow_limit: flow_budget.map(|budget| budget.limit()),
            flow_remaining: flow_budget.map(|budget| budget.remaining()),
            trace_sampled: None,
            metadata_entries: None,
            qos: None,
            security_mode: None,
            identity: None,
            peer_identity: None,
        }
    }

    fn from_routing(stage: Stage, context: &RoutingContext<'_, PipelineMessage>) -> Self {
        let execution = context.execution();
        Self {
            stage,
            cancelled: None,
            deadline_ms: None,
            flow_limit: None,
            flow_remaining: None,
            trace_sampled: Some(execution.trace_context().is_sampled()),
            metadata_entries: Some(context.dynamic_metadata().iter().count()),
            qos: context.intent().expected_qos(),
            security_mode: context.intent().security_preference().cloned(),
            identity: execution.identity().cloned(),
            peer_identity: execution.peer_identity().cloned(),
        }
    }

    fn cancel_injected(point: CancelPoint, success: bool) -> Self {
        Self {
            stage: Stage::CancellationInjected(point),
            cancelled: Some(success),
            deadline_ms: None,
            flow_limit: None,
            flow_remaining: None,
            trace_sampled: None,
            metadata_entries: None,
            qos: None,
            security_mode: None,
            identity: None,
            peer_identity: None,
        }
    }
}

/// 执行阶段，用于精确比较事件轨迹。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stage {
    RouterRoute,
    ServicePollReady,
    ServiceCall,
    CodecEncode,
    CodecDecode,
    TransportScheme,
    TransportBind,
    TransportConnect,
    TransportServerLocal,
    CancellationInjected(CancelPoint),
}

/// 取消插入点的枚举，便于差异化诊断。
#[derive(Clone, Debug, PartialEq, Eq)]
enum CancelPoint {
    BeforeRouterRoute,
    BeforeServicePollReady,
    BeforeServiceCall,
    BeforeCodecEncode,
    BeforeTransportBind,
    BeforeTransportConnect,
}

/// 记录型 Service，实现 `poll_ready`/`call` 时捕获上下文快照。
#[derive(Clone)]
struct RecordingServiceHandle {
    core: Arc<RecordingServiceCore>,
}

impl RecordingServiceHandle {
    fn new(events: Arc<Mutex<Vec<StageObservation>>>, flow_budget: Budget) -> Self {
        Self {
            core: Arc::new(RecordingServiceCore {
                events,
                flow_budget,
            }),
        }
    }
}

struct RecordingServiceCore {
    events: Arc<Mutex<Vec<StageObservation>>>,
    flow_budget: Budget,
}

impl RecordingServiceCore {
    fn record_execution(&self, ctx: &Context<'_>) {
        self.events
            .lock()
            .expect("写入阶段快照时锁应可用")
            .push(StageObservation::from_execution(
                Stage::ServicePollReady,
                ctx,
            ));
    }

    fn record_call(&self, ctx: &CallContext) {
        self.events
            .lock()
            .expect("写入阶段快照时锁应可用")
            .push(StageObservation::from_call(Stage::ServiceCall, ctx));
    }
}

impl Service<PipelineMessage> for RecordingServiceHandle {
    type Response = PipelineMessage;
    type Error = SparkError;
    type Future = core::future::Ready<spark_core::Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        ctx: &Context<'_>,
        _: &mut TaskContext<'_>,
    ) -> spark_core::status::PollReady<Self::Error> {
        self.core.record_execution(ctx);
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    }

    fn call(&mut self, ctx: CallContext, req: PipelineMessage) -> Self::Future {
        self.core.record_call(&ctx);
        core::future::ready(Ok(req))
    }
}

/// 记录路由上下文的伪路由器实现。
struct RecordingRouter {
    events: Arc<Mutex<Vec<StageObservation>>>,
    service: RecordingServiceHandle,
    route_id: RouteId,
    catalog: RouteCatalog,
}

impl RecordingRouter {
    fn new(
        events: Arc<Mutex<Vec<StageObservation>>>,
        service: RecordingServiceHandle,
        route_id: RouteId,
        catalog: RouteCatalog,
    ) -> Self {
        Self {
            events,
            service,
            route_id,
            catalog,
        }
    }
}

impl Router<PipelineMessage> for RecordingRouter {
    type Service = RecordingServiceHandle;
    type Error = SparkError;

    fn route(
        &self,
        context: RoutingContext<'_, PipelineMessage>,
    ) -> spark_core::Result<RouteDecision<Self::Service, PipelineMessage>, RouteError<Self::Error>>
    {
        self.events
            .lock()
            .expect("路由阶段锁应可用")
            .push(StageObservation::from_routing(Stage::RouterRoute, &context));

        let binding = RouteBinding::new(
            self.route_id.clone(),
            self.service.clone(),
            context.dynamic_metadata().clone(),
            context.intent().expected_qos(),
            context.intent().security_preference().cloned(),
        );
        Ok(RouteDecision::new(binding, Vec::new()))
    }

    fn snapshot(&self) -> RoutingSnapshot<'_> {
        RoutingSnapshot::new(&self.catalog, 1)
    }

    fn validate(&self, _: &RouteDescriptor) -> spark_core::router::binding::RouteValidation {
        spark_core::router::binding::RouteValidation::new()
    }
}

/// 记录编解码上下文的伪实现。
struct RecordingCodec {
    events: Arc<Mutex<Vec<StageObservation>>>,
    descriptor: CodecDescriptor,
}

impl RecordingCodec {
    fn new(events: Arc<Mutex<Vec<StageObservation>>>) -> Self {
        let descriptor = CodecDescriptor::new(
            ContentType::new("application/test"),
            ContentEncoding::identity(),
        );
        Self { events, descriptor }
    }
}

impl Codec for RecordingCodec {
    type Incoming = PipelineMessage;
    type Outgoing = PipelineMessage;

    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }

    fn encode(
        &self,
        item: &Self::Outgoing,
        ctx: &mut EncodeContext<'_>,
    ) -> spark_core::Result<EncodedPayload, CoreError> {
        self.events
            .lock()
            .expect("编码阶段锁应可用")
            .push(StageObservation::from_encode(Stage::CodecEncode, ctx));
        let mut buffer = Vec::<u8>::new();
        if let PipelineMessage::User(message) = item {
            buffer.extend_from_slice(message.message_kind().as_bytes());
        }
        Ok(EncodedPayload::from_buffer(Box::new(
            TestReadableBuffer::new(buffer),
        )))
    }

    fn decode(
        &self,
        src: &mut ErasedSparkBuf,
        ctx: &mut DecodeContext<'_>,
    ) -> spark_core::Result<DecodeOutcome<Self::Incoming>, CoreError> {
        self.events
            .lock()
            .expect("解码阶段锁应可用")
            .push(StageObservation::from_decode(Stage::CodecDecode, ctx));
        let remaining = src.remaining();
        if remaining > 0 {
            src.advance(remaining)?;
        }
        Ok(DecodeOutcome::Complete(PipelineMessage::from_user(
            TestUserMessage { tag: 0 },
        )))
    }
}

/// 记录传输层上下文的伪实现。
struct RecordingTransportFactory {
    events: Arc<Mutex<Vec<StageObservation>>>,
    addr: TransportSocketAddr,
}

impl RecordingTransportFactory {
    fn new(events: Arc<Mutex<Vec<StageObservation>>>, addr: TransportSocketAddr) -> Self {
        Self { events, addr }
    }
}

impl TransportFactory for RecordingTransportFactory {
    type Channel = TestChannel;
    type Server = TestServerChannel;

    type BindFuture<'a, P>
        = core::future::Ready<spark_core::Result<Self::Server, CoreError>>
    where
        Self: 'a,
        P: spark_core::pipeline::PipelineFactory + Send + Sync + 'static;

    type ConnectFuture<'a>
        = core::future::Ready<spark_core::Result<Self::Channel, CoreError>>
    where
        Self: 'a;

    fn scheme(&self, ctx: &Context<'_>) -> &'static str {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_execution(
                Stage::TransportScheme,
                ctx,
            ));
        "deterministic-transport"
    }

    fn bind<P>(
        &self,
        ctx: &Context<'_>,
        _config: ListenerConfig,
        _pipeline_factory: Arc<P>,
    ) -> Self::BindFuture<'_, P>
    where
        P: spark_core::pipeline::PipelineFactory + Send + Sync + 'static,
        P::Pipeline: Pipeline,
    {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_execution(Stage::TransportBind, ctx));
        core::future::ready(Ok(TestServerChannel::new(
            Arc::clone(&self.events),
            self.addr,
        )))
    }

    fn connect(
        &self,
        ctx: &Context<'_>,
        _intent: ConnectionIntent,
        _discovery: Option<Arc<dyn spark_core::cluster::ServiceDiscovery>>,
    ) -> Self::ConnectFuture<'_> {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_execution(
                Stage::TransportConnect,
                ctx,
            ));
        core::future::ready(Ok(TestChannel::new(
            "channel-ctx".to_string(),
            Arc::clone(&self.events),
            self.addr,
        )))
    }
}

/// 记录 `ServerChannel` 调用的伪实现。
struct TestServerChannel {
    events: Arc<Mutex<Vec<StageObservation>>>,
    addr: TransportSocketAddr,
    initializer_selector: Mutex<Option<Arc<PipelineInitializerSelector>>>,
}

impl TestServerChannel {
    fn new(events: Arc<Mutex<Vec<StageObservation>>>, addr: TransportSocketAddr) -> Self {
        Self {
            events,
            addr,
            initializer_selector: Mutex::new(None),
        }
    }

    /// 生成本地测试用的握手宣告。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 合约测试无需模拟真实网络协商，但仍需要稳定的握手输入，
    ///   以便验证监听器与选择器之间的接口对接；
    /// - 通过返回固定版本与空能力集合，确保测试具备确定性。
    ///
    /// ## 契约（What）
    /// - 返回 [`HandshakeOffer`]，版本固定 `1.0.0`，能力位图为空；
    /// - **前置条件**：无；
    /// - **后置条件**：可直接传入 [`negotiate`] 获得 [`HandshakeOutcome`](spark_core::transport::HandshakeOutcome)。
    fn handshake_offer(&self) -> HandshakeOffer {
        HandshakeOffer::new(
            Version::new(1, 0, 0),
            spark_core::transport::CapabilityBitmap::empty(),
            spark_core::transport::CapabilityBitmap::empty(),
        )
    }

    /// 执行测试用握手协商。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 在无真实网络元数据的情况下，复用 handshake 模块以确保选择器能够读取
    ///   与生产环境一致的 `HandshakeOutcome` 结构；
    /// - 便于在合约测试中覆盖“协商失败”场景，只需调整本方法即可。
    ///
    /// ## 契约（What）
    /// - 当前实现使用同一宣告作为本地与远端输入，`occurred_at` 固定为 0；
    /// - 返回 [`HandshakeOutcome`](spark_core::transport::HandshakeOutcome)，错误直接 panic，
    ///   因为测试期望协商必定成功。
    fn perform_handshake(&self) -> spark_core::transport::HandshakeOutcome {
        let offer = self.handshake_offer();
        negotiate(&offer, &offer, 0, None).expect("test handshake must succeed")
    }
}

impl ServerChannel for TestServerChannel {
    type Error = CoreError;
    type AcceptCtx<'ctx> = CallContext;
    type ShutdownCtx<'ctx> = Context<'ctx>;
    type Connection = TestChannel;

    type AcceptFuture<'ctx>
        =
        core::future::Ready<spark_core::Result<(Self::Connection, TransportSocketAddr), CoreError>>
    where
        Self: 'ctx,
        Self::AcceptCtx<'ctx>: 'ctx;

    type ShutdownFuture<'ctx>
        = core::future::Ready<spark_core::Result<(), CoreError>>
    where
        Self: 'ctx,
        Self::ShutdownCtx<'ctx>: 'ctx;

    fn scheme(&self) -> &'static str {
        "deterministic-transport"
    }

    fn local_addr(&self, ctx: &Context<'_>) -> spark_core::Result<TransportSocketAddr, CoreError> {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_execution(
                Stage::TransportServerLocal,
                ctx,
            ));
        Ok(self.addr)
    }

    fn accept<'ctx>(&'ctx self, ctx: &'ctx Self::AcceptCtx<'ctx>) -> Self::AcceptFuture<'ctx> {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_call(Stage::TransportBind, ctx));

        if let Some(selector) = self
            .initializer_selector
            .lock()
            .expect("初始化器选择器锁应可用")
            .as_ref()
            .cloned()
        {
            let outcome = self.perform_handshake();
            // 说明：在测试桩中仅调用选择器以验证接口连通性，返回值无需装配实际 Pipeline。
            let initializer = selector(&outcome);
            let _descriptor = initializer.descriptor();
        }

        core::future::ready(Ok((
            TestChannel::new(
                "server-channel".to_string(),
                Arc::clone(&self.events),
                self.addr,
            ),
            self.addr,
        )))
    }

    fn set_initializer_selector(&self, selector: Arc<PipelineInitializerSelector>) {
        let mut guard = self
            .initializer_selector
            .lock()
            .expect("初始化器选择器锁应可用");
        *guard = Some(selector);
    }

    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::ShutdownCtx<'ctx>,
        _plan: ListenerShutdown,
    ) -> Self::ShutdownFuture<'ctx> {
        self.events
            .lock()
            .expect("传输阶段锁应可用")
            .push(StageObservation::from_execution(
                Stage::TransportServerLocal,
                ctx,
            ));
        core::future::ready(Ok(()))
    }
}

/// 记录 `Channel` 调用的伪实现。
struct TestChannel {
    id: String,
    events: Arc<Mutex<Vec<StageObservation>>>,
    addr: TransportSocketAddr,
    controller: NoopController,
    extensions: NoopExtensions,
}

impl TestChannel {
    fn new(
        id: String,
        events: Arc<Mutex<Vec<StageObservation>>>,
        addr: TransportSocketAddr,
    ) -> Self {
        Self {
            id,
            events,
            addr,
            controller: NoopController,
            extensions: NoopExtensions,
        }
    }
}

impl Channel for TestChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn state(&self) -> ChannelState {
        ChannelState::Active
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn controller(&self) -> &dyn Pipeline<HandleId = PipelineHandleId> {
        &self.controller
    }

    fn extensions(&self) -> &dyn ExtensionsMap {
        &self.extensions
    }

    fn peer_addr(&self) -> Option<TransportSocketAddr> {
        Some(self.addr)
    }

    fn local_addr(&self) -> Option<TransportSocketAddr> {
        Some(self.addr)
    }

    fn close_graceful(&self, _: spark_core::contract::CloseReason, _: Option<Deadline>) {}

    fn close(&self) {}

    fn closed(&self) -> BoxFuture<'static, spark_core::Result<(), SparkError>> {
        Box::pin(async { Ok(()) })
    }

    fn write(&self, _msg: PipelineMessage) -> spark_core::Result<WriteSignal, CoreError> {
        Ok(WriteSignal::Accepted)
    }

    fn flush(&self) {}
}

/// 不记录事件的 ExtensionsMap 空实现。
#[derive(Default)]
struct NoopExtensions;

impl ExtensionsMap for NoopExtensions {
    fn insert(&self, _key: std::any::TypeId, _value: Box<dyn std::any::Any + Send + Sync>) {}

    fn get<'a>(
        &'a self,
        _key: &std::any::TypeId,
    ) -> Option<&'a (dyn std::any::Any + Send + Sync + 'static)> {
        None
    }

    fn remove(&self, _key: &std::any::TypeId) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }

    fn contains_key(&self, _key: &std::any::TypeId) -> bool {
        false
    }

    fn clear(&self) {}
}

/// 不执行任何操作的 Pipeline，实现所有接口为 no-op。
#[derive(Clone, Default)]
struct NoopController;

impl Pipeline for NoopController {
    type HandleId = PipelineHandleId;
    fn register_inbound_handler(
        &self,
        _: &str,
        _: Box<dyn spark_core::pipeline::handler::InboundHandler>,
    ) {
    }

    fn register_outbound_handler(
        &self,
        _: &str,
        _: Box<dyn spark_core::pipeline::handler::OutboundHandler>,
    ) {
    }

    fn install_middleware(
        &self,
        _: &dyn spark_core::pipeline::PipelineInitializer,
        _: &spark_core::runtime::CoreServices,
    ) -> spark_core::Result<(), CoreError> {
        Ok(())
    }

    fn emit_channel_activated(&self) {}

    fn emit_read(&self, _msg: PipelineMessage) {}

    fn emit_read_completed(&self) {}

    fn emit_writability_changed(&self, _is_writable: bool) {}

    fn emit_user_event(&self, _event: spark_core::observability::CoreUserEvent) {}

    fn emit_exception(&self, _error: CoreError) {}

    fn emit_channel_deactivated(&self) {}

    fn registry(&self) -> &dyn HandlerRegistry {
        &NoopRegistry
    }

    fn add_handler_after(
        &self,
        _anchor: Self::HandleId,
        _label: &str,
        _handler: Arc<dyn spark_core::pipeline::controller::Handler>,
    ) -> Self::HandleId {
        spark_core::pipeline::controller::PipelineHandleId::INBOUND_HEAD
    }

    fn remove_handler(&self, _handle: Self::HandleId) -> bool {
        false
    }

    fn replace_handler(
        &self,
        _handle: Self::HandleId,
        _handler: Arc<dyn spark_core::pipeline::controller::Handler>,
    ) -> bool {
        false
    }

    fn epoch(&self) -> u64 {
        0
    }
}

/// 空注册表，返回空快照。
struct NoopRegistry;

impl HandlerRegistry for NoopRegistry {
    fn snapshot(&self) -> Vec<spark_core::pipeline::controller::HandlerRegistration> {
        Vec::new()
    }
}

/// 不会分配缓冲的占位分配器。
struct NoopAllocator;

impl BufferAllocator for NoopAllocator {
    fn acquire(
        &self,
        _min_capacity: usize,
    ) -> spark_core::Result<Box<ErasedSparkBufMut>, CoreError> {
        Err(CoreError::new(
            codes::RUNTIME_INTERNAL,
            "allocator disabled for deterministic test",
        ))
    }
}

/// 简化的只读缓冲实现，存储一次性字节片段。
struct TestReadableBuffer {
    data: Vec<u8>,
    cursor: usize,
}

impl TestReadableBuffer {
    fn new(data: Vec<u8>) -> Self {
        Self { data, cursor: 0 }
    }
}

impl ReadableBuffer for TestReadableBuffer {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }

    fn chunk(&self) -> &[u8] {
        &self.data[self.cursor..]
    }

    fn split_to(&mut self, len: usize) -> spark_core::Result<Box<dyn ReadableBuffer>, CoreError> {
        if len > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "split exceeds remaining bytes",
            ));
        }
        let end = self.cursor + len;
        let slice = self.data[self.cursor..end].to_vec();
        self.cursor = end;
        Ok(Box::new(TestReadableBuffer::new(slice)))
    }

    fn advance(&mut self, len: usize) -> spark_core::Result<(), CoreError> {
        if len > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "advance exceeds remaining bytes",
            ));
        }
        self.cursor += len;
        Ok(())
    }

    fn copy_into_slice(&mut self, dst: &mut [u8]) -> spark_core::Result<(), CoreError> {
        if dst.len() > self.remaining() {
            return Err(CoreError::new(
                codes::PROTOCOL_DECODE,
                "insufficient data for copy",
            ));
        }
        let end = self.cursor + dst.len();
        dst.copy_from_slice(&self.data[self.cursor..end]);
        self.cursor = end;
        Ok(())
    }

    fn try_into_vec(self: Box<Self>) -> spark_core::Result<Vec<u8>, CoreError> {
        Ok(self.data)
    }
}

/// 用于 `PipelineMessage` 的测试载荷。
#[derive(Debug)]
struct TestUserMessage {
    tag: u64,
}

/// 注入取消信号并记录事件。
fn maybe_trigger_cancel(
    events: &Arc<Mutex<Vec<StageObservation>>>,
    current_stage: usize,
    cancel_stage: usize,
    cancellation: &mut Cancellation,
    point: CancelPoint,
) {
    if current_stage == cancel_stage && cancellation.cancel() {
        events
            .lock()
            .expect("取消事件锁应可用")
            .push(StageObservation::cancel_injected(point, true));
    }
}

/// 创建最小 waker，供 `poll_ready`/`Future::poll` 使用。
fn noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// 空控制器工厂，仅用于满足 trait 约束。
struct DummyPipelineFactory;

impl spark_core::pipeline::PipelineFactory for DummyPipelineFactory {
    type Pipeline = NoopController;

    fn build(
        &self,
        _: &spark_core::runtime::CoreServices,
    ) -> spark_core::Result<Self::Pipeline, CoreError> {
        Ok(NoopController)
    }
}
