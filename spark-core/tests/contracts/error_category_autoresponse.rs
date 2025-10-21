use std::{
    any::Any,
    sync::{Arc, Mutex},
    time::Duration,
};

use spark_core::contract::{Budget, BudgetKind, CallContext, CallContextBuilder, Cancellation, CloseReason};
use spark_core::error::{CoreError, ErrorCategory};
use spark_core::observability::{metrics::InstrumentDescriptor, AttributeSet, CoreUserEvent, LogRecord, Logger};
use spark_core::pipeline::{
    default_handlers::{ExceptionAutoResponder, ReadyStateEvent},
    Context, Controller, HandlerRegistry,
};
use spark_core::observability::metrics::{Counter, Gauge, Histogram, MetricsProvider};
use spark_core::status::{ReadyState, RetryAdvice};
use spark_core::{
    buffer::{PoolStatDimension, PoolStats, PipelineMessage},
    contract::Deadline,
    future::BoxFuture,
    observability::{TraceContext, TraceFlags},
    pipeline::{channel::ChannelState, Channel, WriteSignal},
    runtime::{
        JoinHandle, MonotonicTimePoint, TaskCancellationStrategy, TaskError, TaskExecutor,
        TaskHandle, TaskResult, TimeDriver,
    },
};

#[test]
fn resource_exhausted_produces_budget_ready_state() {
    let controller = Arc::new(RecordingController::default());
    let budget = Budget::new(BudgetKind::Flow, 1);
    let _ = budget.try_consume(1);
    let call_context = CallContext::builder().add_budget(budget).build();
    let ctx = RecordingContext::new(controller.clone(), call_context);
    let error = CoreError::new("test.resource", "budget exhausted")
        .with_category(ErrorCategory::ResourceExhausted(BudgetKind::Flow));

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    let states = controller.ready_states();
    assert_eq!(states.len(), 1, "ResourceExhausted 应广播一个 ReadyState 事件");
    match &states[0] {
        ReadyState::BudgetExhausted(snapshot) => {
            assert_eq!(snapshot.limit, 1);
            assert_eq!(snapshot.remaining, 0);
        }
        state => panic!("expect budget exhausted but got {:?}", state),
    }
}

#[test]
fn retryable_maps_to_retry_after_signal() {
    let controller = Arc::new(RecordingController::default());
    let call_context = CallContext::builder().build();
    let ctx = RecordingContext::new(controller.clone(), call_context);
    let advice = RetryAdvice::after(Duration::from_millis(150));
    let error = CoreError::new("test.retry", "try later").with_category(ErrorCategory::Retryable(advice.clone()));

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    let states = controller.ready_states();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0], ReadyState::RetryAfter(advice));
}

#[test]
fn security_violation_triggers_graceful_close() {
    let controller = Arc::new(RecordingController::default());
    let call_context = CallContext::builder().build();
    let ctx = RecordingContext::new(controller, call_context);
    let error = CoreError::new("test.security", "unauthorized")
        .with_category(ErrorCategory::Security(spark_core::security::SecurityClass::Authorization));

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    let reasons = ctx.channel.closed_reasons();
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code(), "security.violation");
    assert!(reasons[0].message().contains("权限不足"));
}

#[test]
fn protocol_violation_forces_shutdown() {
    let controller = Arc::new(RecordingController::default());
    let call_context = CallContext::builder().build();
    let ctx = RecordingContext::new(controller, call_context);
    let error = CoreError::new("test.protocol", "bad frame").with_category(ErrorCategory::ProtocolViolation);

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    let reasons = ctx.channel.closed_reasons();
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code(), "protocol.violation");
}

#[test]
fn cancelled_marks_cancellation_token() {
    let controller = Arc::new(RecordingController::default());
    let cancellation = Cancellation::new();
    let call_context = CallContextBuilder::default()
        .with_cancellation(cancellation.clone())
        .build();
    let ctx = RecordingContext::new(controller, call_context);
    let error = CoreError::new("test.cancelled", "cancelled").with_category(ErrorCategory::Cancelled);

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    assert!(cancellation.is_cancelled(), "Cancelled 分类必须标记取消");
}

#[test]
fn timeout_marks_cancellation_token() {
    let controller = Arc::new(RecordingController::default());
    let cancellation = Cancellation::new();
    let call_context = CallContextBuilder::default()
        .with_cancellation(cancellation.clone())
        .build();
    let ctx = RecordingContext::new(controller, call_context);
    let error = CoreError::new("test.timeout", "timeout").with_category(ErrorCategory::Timeout);

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    assert!(cancellation.is_cancelled(), "Timeout 分类必须标记取消");
}

#[test]
fn non_retryable_performs_no_side_effects() {
    let controller = Arc::new(RecordingController::default());
    let cancellation = Cancellation::new();
    let call_context = CallContextBuilder::default()
        .with_cancellation(cancellation.clone())
        .build();
    let ctx = RecordingContext::new(controller.clone(), call_context);
    let error = CoreError::new("test.other", "noop").with_category(ErrorCategory::NonRetryable);

    ExceptionAutoResponder::new().on_exception_caught(&ctx, error);

    assert!(controller.ready_states().is_empty());
    assert!(ctx.channel.closed_reasons().is_empty());
    assert!(!cancellation.is_cancelled());
}

#[derive(Default)]
struct RecordingController {
    ready_states: Mutex<Vec<ReadyState>>,
    registry: EmptyRegistry,
}

impl RecordingController {
    fn ready_states(&self) -> Vec<ReadyState> {
        self.ready_states.lock().unwrap().clone()
    }
}

impl Controller for RecordingController {
    fn register_inbound_handler(&self, _: &str, _: Box<dyn spark_core::pipeline::InboundHandler>) {}

    fn register_outbound_handler(&self, _: &str, _: Box<dyn spark_core::pipeline::OutboundHandler>) {}

    fn install_middleware(
        &self,
        _: &dyn spark_core::pipeline::Middleware,
        _: &spark_core::runtime::CoreServices,
    ) -> Result<(), CoreError> {
        Ok(())
    }

    fn emit_channel_activated(&self) {}

    fn emit_read(&self, _msg: PipelineMessage) {}

    fn emit_read_completed(&self) {}

    fn emit_writability_changed(&self, _is_writable: bool) {}

    fn emit_user_event(&self, event: CoreUserEvent) {
        if let Some(ready) = event.downcast_application_event::<ReadyStateEvent>() {
            self.ready_states.lock().unwrap().push(ready.state().clone());
        }
    }

    fn emit_exception(&self, _error: CoreError) {}

    fn emit_channel_deactivated(&self) {}

    fn registry(&self) -> &dyn HandlerRegistry {
        &self.registry
    }
}

#[derive(Default)]
struct EmptyRegistry;

impl HandlerRegistry for EmptyRegistry {
    fn snapshot(&self) -> Vec<spark_core::pipeline::controller::HandlerRegistration> {
        Vec::new()
    }
}

#[derive(Default)]
struct RecordingChannel {
    controller: Arc<RecordingController>,
    closed: Mutex<Vec<CloseReason>>,
    extensions: NoopExtensions,
}

impl RecordingChannel {
    fn new(controller: Arc<RecordingController>) -> Self {
        Self {
            controller,
            closed: Mutex::new(Vec::new()),
            extensions: NoopExtensions::default(),
        }
    }

    fn closed_reasons(&self) -> Vec<CloseReason> {
        self.closed.lock().unwrap().clone()
    }
}

impl Channel for RecordingChannel {
    fn id(&self) -> &str {
        "test-channel"
    }

    fn state(&self) -> ChannelState {
        ChannelState::Active
    }

    fn is_writable(&self) -> bool {
        true
    }

    fn controller(&self) -> &dyn Controller {
        &*self.controller
    }

    fn extensions(&self) -> &dyn spark_core::pipeline::ExtensionsMap {
        &self.extensions
    }

    fn peer_addr(&self) -> Option<spark_core::transport::TransportSocketAddr> {
        None
    }

    fn local_addr(&self) -> Option<spark_core::transport::TransportSocketAddr> {
        None
    }

    fn close_graceful(&self, reason: CloseReason, _deadline: Option<Deadline>) {
        self.closed.lock().unwrap().push(reason);
    }

    fn close(&self) {}

    fn closed(&self) -> BoxFuture<'static, Result<(), spark_core::SparkError>> {
        Box::pin(async { Ok(()) })
    }

    fn write(&self, _msg: PipelineMessage) -> Result<WriteSignal, CoreError> {
        Ok(WriteSignal::Accepted)
    }

    fn flush(&self) {}
}

#[derive(Default)]
struct NoopExtensions;

impl spark_core::pipeline::ExtensionsMap for NoopExtensions {
    fn insert(&self, _: std::any::TypeId, _: Box<dyn std::any::Any + Send + Sync>) {}

    fn get<'a>(
        &'a self,
        _: &std::any::TypeId,
    ) -> Option<&'a (dyn std::any::Any + Send + Sync + 'static)> {
        None
    }

    fn remove(&self, _: &std::any::TypeId) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        None
    }

    fn contains_key(&self, _: &std::any::TypeId) -> bool {
        false
    }

    fn clear(&self) {}
}

struct RecordingContext {
    controller: Arc<RecordingController>,
    channel: RecordingChannel,
    buffer_pool: NoopBufferPool,
    logger: NoopLogger,
    metrics: NoopMetricsProvider,
    executor: NoopTaskExecutor,
    timer: NoopTimeDriver,
    call_context: CallContext,
    trace: TraceContext,
}

impl RecordingContext {
    fn new(controller: Arc<RecordingController>, call_context: CallContext) -> Self {
        let trace = TraceContext::new(
            [1; TraceContext::TRACE_ID_LENGTH],
            [2; TraceContext::SPAN_ID_LENGTH],
            TraceFlags::default(),
        );
        Self {
            channel: RecordingChannel::new(controller.clone()),
            controller,
            buffer_pool: NoopBufferPool,
            logger: NoopLogger,
            metrics: NoopMetricsProvider::default(),
            executor: NoopTaskExecutor,
            timer: NoopTimeDriver,
            call_context,
            trace,
        }
    }
}

impl Context for RecordingContext {
    fn channel(&self) -> &dyn Channel {
        &self.channel
    }

    fn controller(&self) -> &dyn Controller {
        &*self.controller
    }

    fn executor(&self) -> &dyn TaskExecutor {
        &self.executor
    }

    fn timer(&self) -> &dyn TimeDriver {
        &self.timer
    }

    fn buffer_pool(&self) -> &dyn spark_core::buffer::BufferPool {
        &self.buffer_pool
    }

    fn trace_context(&self) -> &TraceContext {
        &self.trace
    }

    fn metrics(&self) -> &dyn MetricsProvider {
        &self.metrics
    }

    fn logger(&self) -> &dyn Logger {
        &self.logger
    }

    fn membership(&self) -> Option<&dyn spark_core::cluster::ClusterMembership> {
        None
    }

    fn discovery(&self) -> Option<&dyn spark_core::cluster::ServiceDiscovery> {
        None
    }

    fn call_context(&self) -> &CallContext {
        &self.call_context
    }

    fn forward_read(&self, _msg: PipelineMessage) {}

    fn write(&self, msg: PipelineMessage) -> Result<WriteSignal, CoreError> {
        self.channel.write(msg)
    }

    fn flush(&self) {
        self.channel.flush();
    }

    fn close_graceful(&self, reason: CloseReason, deadline: Option<Deadline>) {
        self.channel.close_graceful(reason, deadline);
    }

    fn closed(&self) -> BoxFuture<'static, Result<(), spark_core::SparkError>> {
        self.channel.closed()
    }
}

struct NoopBufferPool;

impl spark_core::buffer::BufferPool for NoopBufferPool {
    fn acquire(&self, _: usize) -> Result<Box<dyn spark_core::buffer::WritableBuffer>, CoreError> {
        Err(CoreError::new("buffer.disabled", "buffer pool disabled"))
    }

    fn shrink_to_fit(&self) -> Result<usize, CoreError> {
        Ok(0)
    }

    fn statistics(&self) -> Result<PoolStats, CoreError> {
        Ok(PoolStats {
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

struct NoopLogger;

impl Logger for NoopLogger {
    fn log(&self, _record: &spark_core::observability::LogRecord<'_>) {}
}

#[derive(Default)]
struct NoopMetricsProvider;

impl MetricsProvider for NoopMetricsProvider {
    fn counter(&self, _: &InstrumentDescriptor<'_>) -> Arc<dyn Counter> {
        Arc::new(NoopCounter)
    }

    fn gauge(&self, _: &InstrumentDescriptor<'_>) -> Arc<dyn Gauge> {
        Arc::new(NoopGauge)
    }

    fn histogram(&self, _: &InstrumentDescriptor<'_>) -> Arc<dyn Histogram> {
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

/// `NoopTaskExecutor` 作为占位执行器，为契约测试提供最小依赖。
///
/// # 设计背景（Why）
/// - 错误分类自动响应逻辑与任务调度解耦，测试中无需拉起完整运行时。
/// - 维持对 `TaskExecutor` 契约的实现，确保接口变更能第一时间在测试中暴露。
///
/// # 逻辑解析（How）
/// - 所有任务提交入口统一转化为 `spawn_dyn`，并返回立即失败的句柄。
/// - 忽略上下文与任务内容，避免引入线程或调度复杂度。
///
/// # 契约说明（What）
/// - **前置条件**：调用方不得依赖任务执行结果。
/// - **后置条件**：所有提交都会返回 [`TaskError::ExecutorTerminated`]。
///
/// # 风险提示（Trade-offs）
/// - 无法检测真实运行时的上下文传播；如需覆盖该路径，请在集成测试中替换实现。
struct NoopTaskExecutor;

impl TaskExecutor for NoopTaskExecutor {
    fn spawn_dyn(
        &self,
        _ctx: &CallContext,
        _fut: spark_core::BoxFuture<'static, TaskResult<Box<dyn Any + Send>>>,
    ) -> JoinHandle<Box<dyn Any + Send>> {
        // 教案级说明（Why）
        // - 合同层单元测试仅关注状态转换，异步执行器只需提供占位实现即可。
        // - 提前返回错误，避免误导调用方认为任务已执行。
        //
        // 教案级说明（How）
        // - 丢弃 `CallContext` 与 Future，构造 `NoopTaskHandle`。
        // - 借助 [`JoinHandle::from_task_handle`] 保持接口一致性。
        //
        // 教案级说明（What）
        // - 输出：立即失败的 [`JoinHandle`]。
        //
        // 教案级说明（Trade-offs）
        // - 不覆盖真正的任务调度路径，但换取了测试的可维护性与确定性。
        JoinHandle::from_task_handle(Box::new(NoopTaskHandle))
    }
}

/// `NoopTaskHandle` 对应立即失败的任务句柄。
///
/// # 设计背景（Why）
/// - 搭配 `NoopTaskExecutor` 一起使用，确保所有运行时调用都能顺利编译而无需调度线程。
///
/// # 逻辑解析（How）
/// - 固定输出类型为 `Box<dyn Any + Send>`，满足对象安全要求。
/// - `join` 直接返回 [`TaskError::ExecutorTerminated`]，其余查询均为恒值。
///
/// # 契约说明（What）
/// - **前置条件**：任务从未开始执行。
/// - **后置条件**：消费句柄不会对外部状态造成影响。
///
/// # 风险提示（Trade-offs）
/// - 不适合需要校验任务完成路径的测试，应在相应场景替换为更真实的实现。
#[derive(Default)]
struct NoopTaskHandle;

#[async_trait::async_trait]
impl TaskHandle for NoopTaskHandle {
    type Output = Box<dyn Any + Send>;

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
        Err(TaskError::ExecutorTerminated)
    }
}

struct NoopTimeDriver;

impl TimeDriver for NoopTimeDriver {
    fn now(&self) -> MonotonicTimePoint {
        MonotonicTimePoint::from_offset(Duration::from_millis(0))
    }

    async fn sleep(&self, _: Duration) {}
}
