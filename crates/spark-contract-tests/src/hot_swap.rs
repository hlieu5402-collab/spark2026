use crate::case::{TckCase, TckSuite};
use crate::support::shared_vec;
use parking_lot::Mutex;
use spark_core::SparkError;
use spark_core::buffer::{BufferPool, PipelineMessage, WritableBuffer};
use spark_core::contract::{CallContext, CloseReason, Deadline};
use spark_core::future::BoxFuture;
use spark_core::observability::{
    AttributeSet, Counter, EventPolicy, Gauge, Histogram, LogRecord, Logger, MetricsProvider,
    OpsEvent, OpsEventBus, OpsEventKind, TraceContext, TraceFlags,
};
use spark_core::pipeline::channel::{ChannelState, WriteSignal};
use spark_core::pipeline::controller::{
    ControllerHandleId, HotSwapController, handler_from_inbound,
};
use spark_core::pipeline::handler::InboundHandler;
use spark_core::pipeline::{Channel, Controller, ExtensionsMap};
use spark_core::runtime::{
    AsyncRuntime, CoreServices, MonotonicTimePoint, TaskCancellationStrategy, TaskExecutor,
    TaskHandle, TaskResult, TimeDriver,
};
use std::any::TypeId;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

const CASES: &[TckCase] = &[TckCase {
    name: "hot_swap_inserts_handler_without_dropping_messages",
    test: hot_swap_inserts_handler_without_dropping_messages,
}];

const SUITE: TckSuite = TckSuite {
    name: "hot_swap",
    cases: CASES,
};

/// 返回“热插拔”主题的测试套件。
///
/// # 教案式说明
/// - **意图 (Why)**：验证 Pipeline 在运行期插入 Handler 时的顺序性与状态维护。
/// - **逻辑 (How)**：目前包含单用例，关注 Handler 注册、重放与事件记录。
/// - **契约 (What)**：返回 `'static` 引用供宏调用。
pub const fn suite() -> &'static TckSuite {
    &SUITE
}

/// 验证 `HotSwapController` 在链路中插入新 Handler 时不会丢失消息或破坏顺序。
///
/// # 教案式说明
/// - **意图 (Why)**：运行期升级 Transport/Protocol 时需要动态插入 Handler，必须保证已有流量不受影响。
/// - **逻辑 (How)**：构造只记录事件的通道，先注册 TLS Handler，再在其后插入 Logging Handler，观察事件顺序与 epoch。
/// - **契约 (What)**：
///   - 插入前事件序列为 `tls:1`；
///   - 插入后事件序列为 `tls:1`, `tls:2`, `log:2`；
///   - Controller 的 epoch 自增，新增句柄不指向链表头。
fn hot_swap_inserts_handler_without_dropping_messages() {
    let runtime = Arc::new(NoopRuntime::new());
    let logger = Arc::new(NoopLogger);
    let ops = Arc::new(NoopOpsBus::default());
    let metrics = Arc::new(NoopMetrics);

    let services = CoreServices {
        runtime: runtime as Arc<dyn AsyncRuntime>,
        buffer_pool: Arc::new(NoopBufferPool),
        metrics: metrics as Arc<dyn MetricsProvider>,
        logger: logger as Arc<dyn Logger>,
        membership: None,
        discovery: None,
        ops_bus: ops as Arc<dyn OpsEventBus>,
        health_checks: Arc::new(Vec::new()),
    };

    let call_context = CallContext::builder().build();
    let trace_context = TraceContext::new(
        [0x11; TraceContext::TRACE_ID_LENGTH],
        [0x22; TraceContext::SPAN_ID_LENGTH],
        TraceFlags::new(TraceFlags::SAMPLED),
    );

    let channel = Arc::new(TestChannel::new("hot-swap-channel"));
    let controller = Arc::new(HotSwapController::new(
        channel.clone() as Arc<dyn Channel>,
        services,
        call_context,
        trace_context,
    ));
    channel
        .bind_controller(controller.clone() as Arc<dyn Controller<HandleId = ControllerHandleId>>);

    let events = shared_vec();
    controller.register_inbound_handler(
        "tls",
        Box::new(RecordingInbound::new("tls", Arc::clone(&events))),
    );

    controller.emit_read(PipelineMessage::from_user(TestMessage { id: 1 }));
    {
        let recorded = events.lock();
        assert_eq!(recorded.as_slice(), ["tls:1"], "插入前仅 TLS 处理消息");
    }

    let tls_handle = controller
        .registry()
        .snapshot()
        .into_iter()
        .find(|entry| entry.label() == "tls")
        .expect("应能在注册表中找到 TLS Handler")
        .handle_id();

    let logging_handler: Arc<dyn InboundHandler> =
        Arc::new(RecordingInbound::new("log", Arc::clone(&events)));
    let dyn_handler = handler_from_inbound(logging_handler);
    let epoch_before = controller.epoch();
    let logging_handle = controller.add_handler_after(tls_handle, "logging", dyn_handler);
    assert_ne!(logging_handle, ControllerHandleId::INBOUND_HEAD);
    assert!(controller.epoch() > epoch_before);

    controller.emit_read(PipelineMessage::from_user(TestMessage { id: 2 }));

    let recorded = events.lock();
    assert_eq!(recorded.as_slice(), ["tls:1", "tls:2", "log:2"]);
}

/// 测试专用的 Channel，实现最小接口以绑定 `HotSwapController`。
struct TestChannel {
    id: String,
    controller: OnceLock<Arc<dyn Controller<HandleId = ControllerHandleId>>>,
    extensions: NoopExtensions,
}

impl TestChannel {
    /// 构造带指定 ID 的测试通道。
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            controller: OnceLock::new(),
            extensions: NoopExtensions,
        }
    }

    /// 绑定控制器供 Pipeline 回调使用。
    fn bind_controller(&self, controller: Arc<dyn Controller<HandleId = ControllerHandleId>>) {
        if self.controller.set(controller).is_err() {
            panic!("controller should only be bound once");
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

    fn controller(&self) -> &dyn Controller<HandleId = ControllerHandleId> {
        self.controller
            .get()
            .expect("controller should be bound")
            .as_ref()
    }

    fn extensions(&self) -> &dyn ExtensionsMap {
        &self.extensions
    }

    fn peer_addr(&self) -> Option<spark_core::transport::TransportSocketAddr> {
        None
    }

    fn local_addr(&self) -> Option<spark_core::transport::TransportSocketAddr> {
        None
    }

    fn close_graceful(&self, _reason: CloseReason, _deadline: Option<Deadline>) {}

    fn close(&self) {}

    fn closed(&self) -> BoxFuture<'static, Result<(), SparkError>> {
        Box::pin(async { Ok(()) })
    }

    fn write(&self, _msg: PipelineMessage) -> Result<WriteSignal, spark_core::error::CoreError> {
        Ok(WriteSignal::Accepted)
    }

    fn flush(&self) {}
}

#[derive(Copy, Clone)]
struct NoopExtensions;

impl ExtensionsMap for NoopExtensions {
    fn insert(&self, _: TypeId, _: Box<dyn std::any::Any + Send + Sync>) {}

    fn get<'a>(&'a self, _: &TypeId) -> Option<&'a (dyn std::any::Any + Send + Sync + 'static)> {
        None
    }

    fn remove(&self, _: &TypeId) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }

    fn contains_key(&self, _: &TypeId) -> bool {
        false
    }

    fn clear(&self) {}
}

/// 仅记录事件顺序的 Inbound Handler，用于验证热插拔行为。
struct RecordingInbound {
    name: &'static str,
    events: Arc<Mutex<Vec<String>>>,
}

impl RecordingInbound {
    /// 创建记录器并持有共享事件缓冲。
    fn new(name: &'static str, events: Arc<Mutex<Vec<String>>>) -> Self {
        Self { name, events }
    }
}

impl InboundHandler for RecordingInbound {
    fn on_channel_active(&self, _ctx: &dyn spark_core::pipeline::Context) {}

    fn on_read(&self, ctx: &dyn spark_core::pipeline::Context, msg: PipelineMessage) {
        match msg.try_into_user::<TestMessage>() {
            Ok(user) => {
                self.events
                    .lock()
                    .push(format!("{}:{}", self.name, user.id));
                ctx.forward_read(PipelineMessage::from_user(user));
            }
            Err(other) => ctx.forward_read(other),
        }
    }

    fn on_read_complete(&self, _ctx: &dyn spark_core::pipeline::Context) {}

    fn on_writability_changed(&self, _ctx: &dyn spark_core::pipeline::Context, _is_writable: bool) {
    }

    fn on_user_event(
        &self,
        _ctx: &dyn spark_core::pipeline::Context,
        _event: spark_core::observability::CoreUserEvent,
    ) {
    }

    fn on_exception_caught(
        &self,
        _ctx: &dyn spark_core::pipeline::Context,
        _error: spark_core::error::CoreError,
    ) {
    }

    fn on_channel_inactive(&self, _ctx: &dyn spark_core::pipeline::Context) {}
}

#[derive(Clone, Copy)]
struct TestMessage {
    id: usize,
}

#[derive(Default)]
struct NoopOpsBus {
    events: Mutex<Vec<OpsEvent>>,
    policies: Mutex<Vec<(OpsEventKind, EventPolicy)>>,
}

impl OpsEventBus for NoopOpsBus {
    fn broadcast(&self, event: OpsEvent) {
        self.events.lock().push(event);
    }

    fn subscribe(&self) -> spark_core::BoxStream<'static, OpsEvent> {
        Box::pin(EmptyStream)
    }

    fn set_event_policy(&self, kind: OpsEventKind, policy: EventPolicy) {
        self.policies.lock().push((kind, policy));
    }
}

struct EmptyStream;

impl spark_core::Stream for EmptyStream {
    type Item = OpsEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

#[derive(Default)]
struct NoopMetrics;

impl MetricsProvider for NoopMetrics {
    fn counter(&self, _: &spark_core::observability::InstrumentDescriptor<'_>) -> Arc<dyn Counter> {
        Arc::new(NoopCounter)
    }

    fn gauge(&self, _: &spark_core::observability::InstrumentDescriptor<'_>) -> Arc<dyn Gauge> {
        Arc::new(NoopGauge)
    }

    fn histogram(
        &self,
        _: &spark_core::observability::InstrumentDescriptor<'_>,
    ) -> Arc<dyn Histogram> {
        Arc::new(NoopHistogram)
    }
}

struct NoopCounter;

impl Counter for NoopCounter {
    fn add(&self, _: u64, _: AttributeSet<'_>) {}
}

struct NoopGauge;

impl Gauge for NoopGauge {
    fn set(&self, _: f64, _: AttributeSet<'_>) {}

    fn increment(&self, _: f64, _: AttributeSet<'_>) {}

    fn decrement(&self, _: f64, _: AttributeSet<'_>) {}
}

struct NoopHistogram;

impl Histogram for NoopHistogram {
    fn record(&self, _: f64, _: AttributeSet<'_>) {}
}

struct NoopLogger;

impl Logger for NoopLogger {
    fn log(&self, _record: &LogRecord<'_>) {}
}

struct NoopBufferPool;

impl BufferPool for NoopBufferPool {
    fn acquire(&self, _: usize) -> Result<Box<dyn WritableBuffer>, spark_core::error::CoreError> {
        Err(spark_core::error::CoreError::new(
            "buffer.disabled",
            "buffer pool disabled",
        ))
    }

    fn shrink_to_fit(&self) -> Result<usize, spark_core::error::CoreError> {
        Ok(0)
    }

    fn statistics(&self) -> Result<spark_core::buffer::PoolStats, spark_core::error::CoreError> {
        Ok(spark_core::buffer::PoolStats {
            allocated_bytes: 0,
            resident_bytes: 0,
            active_leases: 0,
            available_bytes: 0,
            pending_lease_requests: 0,
            failed_acquisitions: 0,
            custom_dimensions: Vec::new(),
        })
    }
}

struct NoopRuntime;

impl NoopRuntime {
    fn new() -> Self {
        Self
    }
}

impl TaskExecutor for NoopRuntime {
    fn spawn_dyn(
        &self,
        _: &CallContext,
        _: spark_core::BoxFuture<'static, TaskResult<Box<dyn std::any::Any + Send>>>,
    ) -> spark_core::runtime::JoinHandle<Box<dyn std::any::Any + Send>> {
        spark_core::runtime::JoinHandle::from_task_handle(Box::new(NoopTaskHandle))
    }
}

#[spark_core::async_trait]
impl TimeDriver for NoopRuntime {
    fn now(&self) -> MonotonicTimePoint {
        MonotonicTimePoint::from_offset(Duration::from_millis(0))
    }

    async fn sleep(&self, _: Duration) {}
}

struct NoopTaskHandle;

#[spark_core::async_trait]
impl TaskHandle for NoopTaskHandle {
    type Output = Box<dyn std::any::Any + Send>;

    fn cancel(&self, _: TaskCancellationStrategy) {}

    fn is_finished(&self) -> bool {
        true
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&str> {
        None
    }

    fn detach(self: Box<Self>) {}

    async fn join(self: Box<Self>) -> TaskResult<Self::Output> {
        Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
    }
}
