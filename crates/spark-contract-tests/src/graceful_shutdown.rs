use crate::case::{TckCase, TckSuite};
use crate::support::monotonic;
use parking_lot::Mutex;
use spark_core::contract::{CloseReason, Deadline};
use spark_core::future::Stream;
use spark_core::host::{GracefulShutdownCoordinator, GracefulShutdownStatus};
use spark_core::observability::{
    AttributeSet, Counter, EventPolicy, Gauge, Histogram, LogRecord, LogSeverity, Logger,
    MetricsProvider, OpsEvent, OpsEventBus, OpsEventKind,
};
use spark_core::pipeline::channel::ChannelState;
use spark_core::pipeline::controller::{
    Controller, ControllerHandleId, HandlerRegistration, HandlerRegistry,
};
use spark_core::pipeline::{Channel, ExtensionsMap};
use spark_core::runtime::{
    AsyncRuntime, CoreServices, JoinHandle, MonotonicTimePoint, TaskCancellationStrategy,
    TaskError, TaskExecutor, TaskHandle, TaskResult, TimeDriver,
};
use spark_core::{BoxStream, SparkError};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;
use std::time::Duration;

const CASES: &[TckCase] = &[
    TckCase {
        name: "channel_fin_invocation_is_forwarded_once",
        test: channel_fin_invocation_is_forwarded_once,
    },
    TckCase {
        name: "channel_half_close_waits_for_remote_ack",
        test: channel_half_close_waits_for_remote_ack,
    },
    TckCase {
        name: "expired_deadline_triggers_immediate_force_close",
        test: expired_deadline_triggers_immediate_force_close,
    },
    TckCase {
        name: "pending_channel_times_out_and_forces_close",
        test: pending_channel_times_out_and_forces_close,
    },
    TckCase {
        name: "channel_half_close_requires_dual_ack_steps",
        test: channel_half_close_requires_dual_ack_steps,
    },
    TckCase {
        name: "channel_closed_future_error_reports_failure",
        test: channel_closed_future_error_reports_failure,
    },
];

const SUITE: TckSuite = TckSuite {
    name: "graceful_shutdown",
    cases: CASES,
};

/// 返回“优雅关闭”主题的测试套件。
///
/// # 教案式说明
/// - **意图 (Why)**：为第三方实现提供针对此前 T19 契约的回归脚本，确保 `GracefulShutdownCoordinator`
///   与 Channel 目标之间的 FIN、半关闭、超时与强制终止语义保持一致。
/// - **逻辑 (How)**：组合四个用例，分别验证 `close_graceful` 调用、`closed()` 等待、截止时间到期时的硬关闭
///   以及 deadline 已过期的即时强制流程。
/// - **契约 (What)**：返回 `'static` 套件描述，供 `run_graceful_shutdown_suite` 与 `#[spark_tck]` 宏使用。
pub const fn suite() -> &'static TckSuite {
    &SUITE
}

/// 验证通过 `GracefulShutdownTarget::for_channel` 注册的通道，协调器会在等待阶段前恰好触发一次 FIN（`close_graceful`）。
///
/// # 教案式说明
/// - **意图 (Why)**：FIN 信号代表写方向优雅终止，必须在任何 `await_closed` 行为之前触达下层传输，以便对端按 TCP/QUIC
///   语义执行半关闭。本用例确保协调器不会遗漏该调用，避免出现“直接等待 closed 导致死锁”的缺陷。
/// - **逻辑 (How)**：
///   1. 利用教学级运行时桩构造 `CoreServices`；
///   2. 注册带计数器的测试通道，记录 `close_graceful`/`close` 调用与截止时间；
///   3. 发起带显式截止时间的优雅关闭；
///   4. 断言 `close_graceful` 仅被调用一次，`close` 从未触发，报告状态为 `Completed`；并验证传播的 deadline 与日志/事件数量。
/// - **契约 (What)**：
///   - 输入：协调器需接收合法的 `CloseReason` 与未来的 `Deadline`；
///   - 前置：通道的 `closed()` 会立即完成，避免额外干扰；
///   - 后置：测试记录的 FIN 计数为 1、强制计数为 0，报告中无 `ForcedTimeout` 条目。
fn channel_fin_invocation_is_forwarded_once() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("fin-channel", controller));
    let recorder = channel.recorder();

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("fin", channel.clone() as Arc<dyn Channel>);

    let reason = CloseReason::new("spark.tck.shutdown.fin", "ensure FIN dispatch");
    let deadline = Deadline::with_timeout(monotonic(10, 0), Duration::from_secs(5));

    let report = block_on(coordinator.shutdown(reason.clone(), Some(deadline)));

    assert_eq!(report.results().len(), 1, "仅注册一个通道应生成单条记录");
    assert_eq!(report.forced_count(), 0, "FIN 成功路径不应触发硬关闭");
    assert!(matches!(
        report.results()[0].status(),
        GracefulShutdownStatus::Completed
    ));

    assert_eq!(
        recorder.graceful_calls.load(Ordering::SeqCst),
        1,
        "close_graceful 必须仅触发一次"
    );
    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        0,
        "正常收敛路径不应调用 close()"
    );

    let reasons = recorder.graceful_reasons.lock();
    assert_eq!(reasons.len(), 1, "记录的 FIN 原因数量应与调用次数一致");
    assert_eq!(reasons[0].code(), reason.code());
    assert_eq!(reasons[0].message(), reason.message());

    let deadlines = recorder.graceful_deadlines.lock();
    assert_eq!(deadlines.len(), 1, "Deadline 传播记录应保持一一对应");
    assert_eq!(
        deadlines[0],
        deadline.instant(),
        "GracefulShutdownTarget::for_channel 应透传 deadline"
    );

    assert_eq!(
        ops.events.lock().len(),
        1,
        "应广播一次 ShutdownTriggered 事件"
    );
    assert!(
        logger
            .records
            .lock()
            .iter()
            .any(|entry| entry.severity == LogSeverity::Info
                && entry.message.contains("graceful shutdown initiated")),
        "INFO 日志应标记停机启动"
    );
}

/// 验证协调器在收到 FIN 后会等待 `closed()`（半关闭确认）完成，而不会提前返回或强制终止。
///
/// # 教案式说明
/// - **意图 (Why)**：半关闭阶段允许对端冲刷剩余数据；若协调器在 `close_graceful` 后立即结束，第三方实现可能丢失末尾数据包。
/// - **逻辑 (How)**：
///   1. 注册 `closed()` 需显式唤醒的通道，并在独立线程执行 `shutdown`；
///   2. 主线程轮询 FIN 计数，确认 FIN 已发送但报告仍未返回；
///   3. 手动触发 `complete_closed()`，模拟对端完成；
///   4. 汇合线程并断言返回状态为 `Completed` 且未调用强制关闭。
/// - **契约 (What)**：
///   - 输入：`Deadline` 为空，表示可无限等待对端完成；
///   - 前置：测试线程在协程完成前调用 `complete_closed()`；
///   - 后置：`closed_completions` 计数为 1、`force_calls` 仍为 0，报告中仅出现 `Completed`。
fn channel_half_close_waits_for_remote_ack() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("half-close", controller));
    let recorder = channel.recorder();

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("half-close", channel.clone() as Arc<dyn Channel>);

    let reason = CloseReason::new("spark.tck.shutdown.half", "await peer confirmation");

    let handle = thread::spawn(move || block_on(coordinator.shutdown(reason, None)));

    // 等待 FIN 调用出现，避免在 `closed` Future 未被监听前提前唤醒。
    while recorder.graceful_calls.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        0,
        "等待阶段不应触发硬关闭"
    );

    // 模拟对端完成半关闭，允许 Future 继续。
    recorder.complete_closed();

    let report = handle.join().expect("shutdown 线程不应 panic");

    assert_eq!(report.results().len(), 1);
    assert!(matches!(
        report.results()[0].status(),
        GracefulShutdownStatus::Completed
    ));
    assert_eq!(
        recorder.closed_completions.load(Ordering::SeqCst),
        1,
        "closed() 应完成一次"
    );
    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        0,
        "整个流程未触发强制关闭"
    );
}

/// 验证当调用方提供的截止时间已过期时，协调器会立即进入强制取消路径，无需等待。
///
/// # 教案式说明
/// - **意图 (Why)**：宿主可能在观测到严重故障后触发“立即停止”，此时 `Deadline` 等于或早于当前时间点，协调器应立刻调用
///   `force_close`，避免继续等待已超期的目标。
/// - **逻辑 (How)**：
///   1. 设定截止时间为 `now`，注册一个始终 Pending 的通道；
///   2. 调用 `shutdown` 并直接等待返回；
///   3. 断言 `force_close` 被调用一次，报告状态为 `ForcedTimeout`，同时未发生额外的 `sleep` 调用。
/// - **契约 (What)**：
///   - 输入：`Deadline::with_timeout(..., Duration::ZERO)`；
///   - 前置：通道 `closed()` 永不完成；
///   - 后置：`sleep_calls` 为空、报告 `forced_count()` 为 1，日志包含 WARN 记录。
fn expired_deadline_triggers_immediate_force_close() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("expired", controller));
    let recorder = channel.recorder();
    recorder.keep_closed_pending();

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("expired", channel.clone() as Arc<dyn Channel>);

    let deadline = Deadline::with_timeout(monotonic(0, 0), Duration::ZERO);
    let report = block_on(coordinator.shutdown(
        CloseReason::new("spark.tck.shutdown.expired", "deadline already expired"),
        Some(deadline),
    ));

    assert_eq!(report.results().len(), 1);
    assert_eq!(report.forced_count(), 1, "到期场景必须统计强制关闭");
    match report.results()[0].status() {
        GracefulShutdownStatus::ForcedTimeout => {}
        other => panic!("预期 ForcedTimeout，实际为 {:?}", other),
    }

    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        1,
        "force_close 应立即被调用"
    );
    assert!(
        runtime.sleep_calls().is_empty(),
        "截止时间已过期时不应调用 sleep"
    );
    assert!(
        logger
            .records
            .lock()
            .iter()
            .any(|entry| entry.severity == LogSeverity::Warn && entry.message.contains("forced")),
        "WARN 日志需提示强制关闭"
    );
}

/// 验证当通道在截止时间内始终未完成时，协调器会等待超时后调用硬关闭并记录相应状态。
///
/// # 教案式说明
/// - **意图 (Why)**：确认超时路径既会推进内部计时器（调用 `sleep`），也会在超时时调用 `force_close` 并输出 WARN，确保实现者
///   不会遗漏硬关闭钩子。
/// - **逻辑 (How)**：
///   1. 注册一个永久 Pending 的通道；
///   2. 设置 1 秒截止时间并执行 `shutdown`；
///   3. 断言返回 `ForcedTimeout`，`sleep` 调用记录包含 1 秒，硬关闭计数增 1。
/// - **契约 (What)**：
///   - 输入：`Deadline::with_timeout(now, 1s)`；
///   - 前置：通道不完成 `closed()`；
///   - 后置：报告 `forced_count()` == 1，`sleep_calls()` 包含 1 秒条目，WARN 日志存在。
fn pending_channel_times_out_and_forces_close() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("timeout", controller));
    let recorder = channel.recorder();
    recorder.keep_closed_pending();

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("timeout", channel.clone() as Arc<dyn Channel>);

    let deadline = Deadline::with_timeout(monotonic(0, 0), Duration::from_secs(1));
    let report = block_on(coordinator.shutdown(
        CloseReason::new("spark.tck.shutdown.timeout", "target stalled"),
        Some(deadline),
    ));

    assert_eq!(report.results().len(), 1);
    assert_eq!(report.forced_count(), 1, "超时场景应计入一次强制关闭");
    match report.results()[0].status() {
        GracefulShutdownStatus::ForcedTimeout => {}
        other => panic!("预期 ForcedTimeout，实际为 {:?}", other),
    }

    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        1,
        "超时后必须调用 close()"
    );
    assert_eq!(
        runtime.sleep_calls(),
        vec![Duration::from_secs(1)],
        "计时器应等待完整的截止时间"
    );
    assert!(
        logger
            .records
            .lock()
            .iter()
            .any(|entry| entry.severity == LogSeverity::Warn && entry.message.contains("forced")),
        "超时路径需记录 WARN 日志"
    );
    assert_eq!(
        recorder.closed_completions.load(Ordering::SeqCst),
        0,
        "强制关闭后 `closed()` Future 不应报告成功"
    );
    assert!(
        recorder.closed_drop_count() >= 1,
        "超时导致的取消必须释放 `closed()` Future 资源"
    );
}

/// 验证 `closed()` Future 只有在“读/写半关闭都确认”后才会完成，确保双向流水线被安全冲刷。
///
/// # 教案式说明
/// - **意图 (Why)**：传输层在 FIN 后仍需等待对端完成读半关闭与写方向的剩余数据同步，协调器必须在两个确认到齐前保持 Pending。
/// - **逻辑 (How)**：
///   1. 将测试通道配置为需要两个“半关闭确认”步骤，并在后台线程执行 `shutdown`；
///   2. 观察到 FIN 已触发后，依次调用 `ack_closed_step`，模拟“写半关闭完成”“读半关闭完成”；
///   3. 在第一次确认后断言线程仍未完成，第二次确认后再收集报告；
///   4. 校验 `GracefulShutdownStatus::Completed`，同时确认未触发硬关闭且资源统计正常。
/// - **契约 (What)**：
///   - 输入：无截止时间，允许无限等待；
///   - 前置：测试需在两个确认步骤之间检查 `JoinHandle::is_finished`；
///   - 后置：`closed_completions == 1`、`force_calls == 0`、`closed_drop_count >= 1`，报告状态为 `Completed`。
fn channel_half_close_requires_dual_ack_steps() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("dual-ack", controller));
    let recorder = channel.recorder();
    recorder.require_closed_steps(2);

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("dual-ack", channel.clone() as Arc<dyn Channel>);

    let handle = thread::spawn(move || {
        block_on(coordinator.shutdown(
            CloseReason::new("spark.tck.shutdown.dual", "await read/write half-close"),
            None,
        ))
    });

    while recorder.graceful_calls.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }

    assert!(!handle.is_finished(), "两个半关闭确认均未到齐前不应返回");

    recorder.ack_closed_step();
    thread::yield_now();
    assert!(!handle.is_finished(), "仅完成一个方向的半关闭时仍应等待");

    recorder.ack_closed_step();
    let report = handle.join().expect("shutdown 线程不应 panic");

    assert_eq!(report.results().len(), 1);
    assert!(matches!(
        report.results()[0].status(),
        GracefulShutdownStatus::Completed
    ));
    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        0,
        "双向确认完成后不应触发硬关闭"
    );
    assert_eq!(
        recorder.closed_completions.load(Ordering::SeqCst),
        1,
        "两个方向确认后应成功完成一次 closed()"
    );
    assert!(
        recorder.closed_drop_count() >= 1,
        "Future 在完成后应被丢弃以释放资源"
    );
}

/// 验证 `closed()` Future 返回错误时，协调器会报告 `Failed`，并确保资源释放且未触发硬关闭。
///
/// # 教案式说明
/// - **意图 (Why)**：异常路径需确保 FIN 已经发送，但对端反馈错误时仍能保持一致的状态统计并释放 `closed()` Future。
/// - **逻辑 (How)**：
///   1. 将测试通道配置为 Pending，并在独立线程执行 `shutdown`；
///   2. 在 FIN 触发后调用 `complete_closed_with_error`，模拟执行器返回 `SparkError`；
///   3. 收集报告并检查状态、日志以及资源计数；
///   4. 断言硬关闭计数保持为 0，确认异常路径不会意外调用 `close()`。
/// - **契约 (What)**：
///   - 输入：`CloseReason` 描述异常收敛；
///   - 前置：必须在返回前调用 `complete_closed_with_error`；
///   - 后置：报告状态为 `Failed`，`closed_failure_count == 1`、`force_calls == 0`、`closed_drop_count >= 1`。
fn channel_closed_future_error_reports_failure() {
    let runtime = Arc::new(TestRuntime::new());
    let logger = Arc::new(TestLogger::default());
    let ops = Arc::new(TestOpsBus::default());
    let metrics = Arc::new(TestMetrics);

    let services = build_core_services(
        Arc::clone(&runtime) as Arc<dyn AsyncRuntime>,
        Arc::clone(&logger) as Arc<dyn Logger>,
        Arc::clone(&ops) as Arc<dyn OpsEventBus>,
        Arc::clone(&metrics) as Arc<dyn MetricsProvider>,
    );

    let controller = Arc::new(NoopController);
    let channel = Arc::new(TestChannel::new("error", controller));
    let recorder = channel.recorder();
    recorder.keep_closed_pending();

    let mut coordinator = GracefulShutdownCoordinator::new(services);
    coordinator.register_channel("error", channel.clone() as Arc<dyn Channel>);

    let handle = thread::spawn(move || {
        block_on(coordinator.shutdown(
            CloseReason::new("spark.tck.shutdown.error", "closed future failed"),
            None,
        ))
    });

    while recorder.graceful_calls.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }

    assert!(!handle.is_finished(), "在错误注入前 shutdown 应保持等待");

    recorder.complete_closed_with_error(SparkError::new(
        "spark.tck.shutdown.closed_error",
        "closed future returned error",
    ));

    let report = handle.join().expect("shutdown 线程不应 panic");

    assert_eq!(report.results().len(), 1);
    match report.results()[0].status() {
        GracefulShutdownStatus::Failed(err) => {
            assert_eq!(err.code(), "spark.tck.shutdown.closed_error");
        }
        other => panic!("预期 Failed，实际为 {:?}", other),
    }
    assert_eq!(
        recorder.force_calls.load(Ordering::SeqCst),
        0,
        "异常路径不应调用硬关闭"
    );
    assert_eq!(
        recorder.closed_failure_count(),
        1,
        "应记录一次 closed() 错误"
    );
    assert_eq!(
        recorder.closed_completions.load(Ordering::SeqCst),
        0,
        "错误场景不应统计成功关闭"
    );
    assert!(
        recorder.closed_drop_count() >= 1,
        "错误返回后 Future 应被释放"
    );
}

/// 教学版 `block_on`，避免引入额外依赖同时展示 Future 轮询机制。
///
/// # 教案式说明
/// - **意图 (Why)**：TCK 需在无 Tokio 依赖的情况下执行异步关闭流程，使用手写执行器帮助读者理解 waker 与轮询语义。
/// - **逻辑 (How)**：构造空操作 waker，使用 `Pin::new_unchecked` 固定 Future，并在循环中持续 `poll` 直至返回 `Ready`。
/// - **契约 (What)**：
///   - 输入：任意 `Future`；
///   - 前置：Future 不得依赖真实唤醒；
///   - 后置：函数返回 Future 结果。
fn block_on<F: Future>(mut future: F) -> F::Output {
    fn noop_raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        RawWaker::new(std::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut future = unsafe { Pin::new_unchecked(&mut future) };
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => break output,
            Poll::Pending => continue,
        }
    }
}

/// 聚合运行时依赖，构造协调器所需的 `CoreServices`。
fn build_core_services(
    runtime: Arc<dyn AsyncRuntime>,
    logger: Arc<dyn Logger>,
    ops: Arc<dyn OpsEventBus>,
    metrics: Arc<dyn MetricsProvider>,
) -> CoreServices {
    CoreServices {
        runtime,
        buffer_pool: Arc::new(NoopBufferPool),
        metrics,
        logger,
        membership: None,
        discovery: None,
        ops_bus: ops,
        health_checks: Arc::new(Vec::new()),
    }
}

/// 运行时桩：记录 `sleep` 调用并提供同步完成的任务句柄。
#[derive(Default)]
struct TestRuntime {
    now: Mutex<Duration>,
    sleep_log: Mutex<Vec<Duration>>,
}

impl TestRuntime {
    fn new() -> Self {
        Self::default()
    }

    fn sleep_calls(&self) -> Vec<Duration> {
        self.sleep_log.lock().clone()
    }
}

impl TaskExecutor for TestRuntime {
    fn spawn_dyn(
        &self,
        _ctx: &spark_core::contract::CallContext,
        fut: spark_core::future::BoxFuture<'static, TaskResult<Box<dyn Any + Send>>>,
    ) -> JoinHandle<Box<dyn Any + Send>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on(fut)));
        let task_result = match result {
            Ok(value) => value,
            Err(_) => Err(TaskError::Panicked),
        };
        JoinHandle::from_task_handle(Box::new(ReadyHandle::new(task_result)))
    }
}

#[spark_core::async_trait]
impl TimeDriver for TestRuntime {
    fn now(&self) -> MonotonicTimePoint {
        MonotonicTimePoint::from_offset(*self.now.lock())
    }

    async fn sleep(&self, duration: Duration) {
        self.sleep_log.lock().push(duration);
        let mut guard = self.now.lock();
        *guard = guard.checked_add(duration).expect("测试时钟不应溢出");
    }
}

/// 即时完成的任务句柄，实现 `TaskHandle` 契约。
struct ReadyHandle<T> {
    result: Mutex<Option<TaskResult<T>>>,
    finished: AtomicBool,
    cancelled: AtomicBool,
}

impl<T> ReadyHandle<T> {
    fn new(result: TaskResult<T>) -> Self {
        Self {
            result: Mutex::new(Some(result)),
            finished: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
        }
    }
}

#[spark_core::async_trait]
impl<T: Send + 'static> TaskHandle for ReadyHandle<T> {
    type Output = T;

    fn cancel(&self, _strategy: TaskCancellationStrategy) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn id(&self) -> Option<&str> {
        None
    }

    fn detach(self: Box<Self>) {}

    async fn join(self: Box<Self>) -> TaskResult<Self::Output> {
        self.result
            .lock()
            .take()
            .expect("ReadyHandle::join called multiple times")
    }
}

/// 记录日志的轻量实现。
#[derive(Default)]
struct TestLogger {
    records: Mutex<Vec<OwnedLog>>,
}

#[derive(Clone)]
struct OwnedLog {
    severity: LogSeverity,
    message: String,
}

impl Logger for TestLogger {
    fn log(&self, record: &LogRecord<'_>) {
        let message = record.message.clone().into_owned();
        let entry = OwnedLog {
            severity: record.severity,
            message,
        };
        self.records.lock().push(entry);
    }
}

/// 记录运维事件的桩实现。
#[derive(Default)]
struct TestOpsBus {
    events: Mutex<Vec<OpsEvent>>,
    policies: Mutex<Vec<(OpsEventKind, EventPolicy)>>,
}

impl OpsEventBus for TestOpsBus {
    fn broadcast(&self, event: OpsEvent) {
        self.events.lock().push(event);
    }

    fn subscribe(&self) -> BoxStream<'static, OpsEvent> {
        Box::pin(EmptyStream)
    }

    fn set_event_policy(&self, kind: OpsEventKind, policy: EventPolicy) {
        self.policies.lock().push((kind, policy));
    }
}

/// 空事件流，占位满足 `subscribe` 接口。
struct EmptyStream;

impl Stream for EmptyStream {
    type Item = OpsEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

/// 空指标实现，避免额外依赖。
#[derive(Default)]
struct TestMetrics;

impl MetricsProvider for TestMetrics {
    fn counter(
        &self,
        _descriptor: &spark_core::observability::InstrumentDescriptor<'_>,
    ) -> Arc<dyn Counter> {
        Arc::new(NoopCounter)
    }

    fn gauge(
        &self,
        _descriptor: &spark_core::observability::InstrumentDescriptor<'_>,
    ) -> Arc<dyn Gauge> {
        Arc::new(NoopGauge)
    }

    fn histogram(
        &self,
        _descriptor: &spark_core::observability::InstrumentDescriptor<'_>,
    ) -> Arc<dyn Histogram> {
        Arc::new(NoopHistogram)
    }
}

struct NoopCounter;
impl Counter for NoopCounter {
    fn add(&self, _value: u64, _attributes: AttributeSet<'_>) {}
}

struct NoopGauge;
impl Gauge for NoopGauge {
    fn set(&self, _value: f64, _attributes: AttributeSet<'_>) {}
    fn increment(&self, _delta: f64, _attributes: AttributeSet<'_>) {}
    fn decrement(&self, _delta: f64, _attributes: AttributeSet<'_>) {}
}

struct NoopHistogram;
impl Histogram for NoopHistogram {
    fn record(&self, _value: f64, _attributes: AttributeSet<'_>) {}
}

/// 协调器无需使用缓冲池，提供 no-op 实现。
struct NoopBufferPool;

impl spark_core::buffer::BufferPool for NoopBufferPool {
    fn acquire(
        &self,
        _min_capacity: usize,
    ) -> Result<Box<dyn spark_core::buffer::WritableBuffer>, spark_core::error::CoreError> {
        Err(spark_core::error::CoreError::new(
            "buffer.disabled",
            "buffer pool disabled in tests",
        ))
    }

    fn shrink_to_fit(&self) -> Result<usize, spark_core::error::CoreError> {
        Ok(0)
    }

    fn statistics(&self) -> Result<spark_core::buffer::PoolStats, spark_core::error::CoreError> {
        Ok(Default::default())
    }
}

/// 记录通道关闭调用的共享状态。
struct ChannelRecorder {
    graceful_calls: AtomicUsize,
    force_calls: AtomicUsize,
    graceful_reasons: Mutex<Vec<CloseReason>>,
    graceful_deadlines: Mutex<Vec<Option<MonotonicTimePoint>>>,
    closed_state: Arc<ManualClosedState>,
    closed_completions: AtomicUsize,
    keep_pending: AtomicBool,
    closed_failures: AtomicUsize,
    closed_drops: AtomicUsize,
    failure: Mutex<Option<SparkError>>,
    result_recorded: AtomicBool,
}

impl ChannelRecorder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            graceful_calls: AtomicUsize::new(0),
            force_calls: AtomicUsize::new(0),
            graceful_reasons: Mutex::new(Vec::new()),
            graceful_deadlines: Mutex::new(Vec::new()),
            closed_state: Arc::new(ManualClosedState::new()),
            closed_completions: AtomicUsize::new(0),
            keep_pending: AtomicBool::new(false),
            closed_failures: AtomicUsize::new(0),
            closed_drops: AtomicUsize::new(0),
            failure: Mutex::new(None),
            result_recorded: AtomicBool::new(false),
        })
    }

    fn complete_closed(&self) {
        self.closed_state.complete();
    }

    fn keep_closed_pending(&self) {
        self.keep_pending.store(true, Ordering::SeqCst);
    }

    fn require_closed_steps(&self, steps: usize) {
        self.keep_pending.store(true, Ordering::SeqCst);
        self.closed_state.set_pending_steps(steps);
    }

    fn ack_closed_step(&self) {
        self.closed_state.ack_step();
    }

    fn complete_closed_with_error(&self, error: SparkError) {
        *self.failure.lock() = Some(error);
        self.closed_state.complete();
    }

    fn closed_failure_count(&self) -> usize {
        self.closed_failures.load(Ordering::SeqCst)
    }

    fn closed_drop_count(&self) -> usize {
        self.closed_drops.load(Ordering::SeqCst)
    }

    fn reset_result_recording(&self) {
        self.result_recorded.store(false, Ordering::SeqCst);
    }

    #[allow(clippy::result_large_err)]
    fn take_closed_result(&self) -> Result<(), SparkError> {
        if self.result_recorded.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        if let Some(error) = self.failure.lock().take() {
            self.closed_failures.fetch_add(1, Ordering::SeqCst);
            Err(error)
        } else {
            self.closed_completions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

/// 手动控制的 `closed()` Future 状态。
struct ManualClosedState {
    completed: AtomicBool,
    waker: Mutex<Option<Waker>>,
    pending_steps: AtomicUsize,
}

impl ManualClosedState {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            waker: Mutex::new(None),
            pending_steps: AtomicUsize::new(0),
        }
    }

    fn register(&self, waker: &Waker) {
        *self.waker.lock() = Some(waker.clone());
    }

    fn complete(&self) {
        if !self.completed.swap(true, Ordering::AcqRel) {
            self.pending_steps.store(0, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().take() {
                waker.wake();
            }
        }
    }

    fn set_pending_steps(&self, steps: usize) {
        self.pending_steps.store(steps, Ordering::SeqCst);
        self.completed.store(false, Ordering::Release);
    }

    fn ack_step(&self) {
        loop {
            let current = self.pending_steps.load(Ordering::Acquire);
            if current == 0 {
                break;
            }

            if self
                .pending_steps
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                if current == 1 {
                    self.complete();
                }
                break;
            }
        }
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }
}

struct ManualClosedFuture {
    state: Arc<ManualClosedState>,
    recorder: Arc<ChannelRecorder>,
}

impl Future for ManualClosedFuture {
    type Output = Result<(), SparkError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.is_completed() {
            return Poll::Ready(self.recorder.take_closed_result());
        }

        if self.recorder.keep_pending.load(Ordering::Acquire) {
            self.state.register(cx.waker());
            if self.state.is_completed() {
                Poll::Ready(self.recorder.take_closed_result())
            } else {
                Poll::Pending
            }
        } else {
            self.state.complete();
            Poll::Ready(self.recorder.take_closed_result())
        }
    }
}

impl Drop for ManualClosedFuture {
    fn drop(&mut self) {
        self.recorder.closed_drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// 通道桩，实现最小接口用于验证关闭语义。
struct TestChannel {
    id: String,
    controller: Arc<NoopController>,
    extensions: NoopExtensions,
    recorder: Arc<ChannelRecorder>,
}

impl TestChannel {
    fn new(id: &str, controller: Arc<NoopController>) -> Self {
        Self {
            id: id.to_string(),
            controller,
            extensions: NoopExtensions,
            recorder: ChannelRecorder::new(),
        }
    }

    fn recorder(&self) -> Arc<ChannelRecorder> {
        Arc::clone(&self.recorder)
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
        &*self.controller
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

    fn close_graceful(&self, reason: CloseReason, deadline: Option<Deadline>) {
        self.recorder.graceful_calls.fetch_add(1, Ordering::SeqCst);
        self.recorder.graceful_reasons.lock().push(reason);
        self.recorder
            .graceful_deadlines
            .lock()
            .push(deadline.and_then(|d| d.instant()));
    }

    fn close(&self) {
        self.recorder.force_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn closed(&self) -> spark_core::future::BoxFuture<'static, Result<(), SparkError>> {
        self.recorder.reset_result_recording();
        Box::pin(ManualClosedFuture {
            state: Arc::clone(&self.recorder.closed_state),
            recorder: Arc::clone(&self.recorder),
        })
    }

    fn write(
        &self,
        _msg: spark_core::buffer::PipelineMessage,
    ) -> Result<spark_core::pipeline::WriteSignal, spark_core::error::CoreError> {
        Ok(spark_core::pipeline::WriteSignal::Accepted)
    }

    fn flush(&self) {}
}

#[derive(Copy, Clone)]
struct NoopExtensions;

impl ExtensionsMap for NoopExtensions {
    fn insert(&self, _: std::any::TypeId, _: Box<dyn Any + Send + Sync>) {}
    fn get<'a>(&'a self, _: &std::any::TypeId) -> Option<&'a (dyn Any + Send + Sync + 'static)> {
        None
    }
    fn remove(&self, _: &std::any::TypeId) -> Option<Box<dyn Any + Send + Sync>> {
        None
    }
    fn contains_key(&self, _: &std::any::TypeId) -> bool {
        false
    }
    fn clear(&self) {}
}

/// 最小化控制器实现，所有方法均为 no-op。
#[derive(Default)]
struct NoopController;

impl Controller for NoopController {
    type HandleId = ControllerHandleId;

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
        _: &dyn spark_core::pipeline::Middleware,
        _: &CoreServices,
    ) -> Result<(), spark_core::error::CoreError> {
        Ok(())
    }

    fn emit_channel_activated(&self) {}

    fn emit_read(&self, _: spark_core::buffer::PipelineMessage) {}

    fn emit_read_completed(&self) {}

    fn emit_writability_changed(&self, _: bool) {}

    fn emit_user_event(&self, _: spark_core::observability::CoreUserEvent) {}

    fn emit_exception(&self, _: spark_core::error::CoreError) {}

    fn emit_channel_deactivated(&self) {}

    fn registry(&self) -> &dyn HandlerRegistry {
        &EmptyRegistry
    }

    fn add_handler_after(
        &self,
        anchor: Self::HandleId,
        _: &str,
        _: Arc<dyn spark_core::pipeline::controller::Handler>,
    ) -> Self::HandleId {
        anchor
    }

    fn remove_handler(&self, _: Self::HandleId) -> bool {
        false
    }

    fn replace_handler(
        &self,
        _: Self::HandleId,
        _: Arc<dyn spark_core::pipeline::controller::Handler>,
    ) -> bool {
        false
    }

    fn epoch(&self) -> u64 {
        0
    }
}

struct EmptyRegistry;

impl HandlerRegistry for EmptyRegistry {
    fn snapshot(&self) -> Vec<HandlerRegistration> {
        Vec::new()
    }
}
