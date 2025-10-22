use crate::case::{TckCase, TckSuite};
use parking_lot::Mutex;
use spark_core::configuration::{
    ChangeEvent, ChangeNotification, ConfigDelta, ConfigKey, ConfigMetadata, ConfigScope,
    ConfigValue, ConfigurationBuilder, ConfigurationError, ConfigurationLayer, ConfigurationSource,
    ConfigurationUpdate, ConfigurationUpdateKind, ProfileDescriptor, ProfileId, ProfileLayering,
    SourceMetadata,
};
use spark_core::future::Stream;
use spark_core::limits::{LimitRuntimeConfig, LimitSettings, ResourceKind};
use spark_core::runtime::{TimeoutRuntimeConfig, TimeoutSettings};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::thread;
use std::time::Duration as StdDuration;

const CASES: &[TckCase] = &[TckCase {
    name: "hot_reload_limits_and_timeouts_is_atomic_case",
    test: hot_reload_limits_and_timeouts_is_atomic_case,
}];

const SUITE: TckSuite = TckSuite {
    name: "hot_reload",
    cases: CASES,
};

/// 返回“热重载”主题的测试套件。
///
/// # 教案式说明
/// - **意图 (Why)**：检验配置热更新在并发读取情况下的原子性与纪元递增特性。
/// - **逻辑 (How)**：当前套件包含一个综合用例，覆盖限额与超时的同步更新流程。
/// - **契约 (What)**：返回 `'static` 引用供宏调用。
pub const fn suite() -> &'static TckSuite {
    &SUITE
}

/// 验证限额与超时配置在热重载时的原子性与线程可见性。
///
/// # 教案式说明
/// - **意图 (Why)**：运行时在动态调参时需确保所有消费者看到一致的配置快照，避免混合旧值与新值。
/// - **逻辑 (How)**：
///   1. 构造测试数据源与配置 builder，建立初始限额/超时；
///   2. 启动读线程不断读取快照；
///   3. 推送两次增量更新，并通过 `wait_for_update` 阻塞直到配置应用；
///   4. 检查纪元递增与最终快照。
/// - **契约 (What)**：每次更新均导致纪元递增，最终快照反映最新配置值，期间读线程始终看到合法数据。
fn hot_reload_limits_and_timeouts_is_atomic_case() {
    let profile = ProfileDescriptor::new(
        ProfileId::new("hotreload"),
        Vec::new(),
        ProfileLayering::BaseFirst,
        "hot reload test",
    );

    let initial_layer = ConfigurationLayer {
        metadata: SourceMetadata::new("inline", 0, None),
        entries: vec![
            (
                limit_key(ResourceKind::Connections),
                ConfigValue::Integer(4_096, ConfigMetadata::default()),
            ),
            (
                request_timeout_key(),
                ConfigValue::Integer(5_000, ConfigMetadata::default()),
            ),
            (
                idle_timeout_key(),
                ConfigValue::Integer(60_000, ConfigMetadata::default()),
            ),
        ],
    };

    let mut builder = ConfigurationBuilder::new().with_profile(profile);
    let (source, queue) = TestSource::new(vec![initial_layer]);
    builder
        .register_source(Box::new(source))
        .expect("register source");

    let outcome = builder.build().expect("build configuration");
    let mut handle = outcome.handle;
    let initial = outcome.initial;

    let limits = Arc::new(LimitRuntimeConfig::new(
        LimitSettings::from_configuration(&initial).expect("parse limits"),
    ));
    let timeouts = Arc::new(TimeoutRuntimeConfig::new(
        TimeoutSettings::from_configuration(&initial).expect("parse timeouts"),
    ));

    let mut watch = Box::pin(handle.watch().expect("watch stream"));

    let running = Arc::new(AtomicBool::new(true));
    let limit_reader = {
        let running = Arc::clone(&running);
        let limits = Arc::clone(&limits);
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let snapshot = limits.snapshot();
                assert!(snapshot.plan(ResourceKind::Connections).limit() > 0);
                thread::yield_now();
            }
        })
    };
    let timeout_reader = {
        let running = Arc::clone(&running);
        let timeouts = Arc::clone(&timeouts);
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                let snapshot = timeouts.snapshot();
                assert!(snapshot.request_timeout() > StdDuration::from_millis(0));
                thread::yield_now();
            }
        })
    };

    let mut limit_epoch = limits.config_epoch();
    let mut timeout_epoch = timeouts.config_epoch();

    {
        let mut events = queue.lock();
        events.push_back(ConfigDelta::Change(ChangeNotification::new(
            1,
            1_700_000_000,
            vec![
                ChangeEvent::Updated {
                    key: limit_key(ResourceKind::Connections),
                    value: ConfigValue::Integer(2_048, ConfigMetadata::default()),
                },
                ChangeEvent::Updated {
                    key: request_timeout_key(),
                    value: ConfigValue::Integer(8_000, ConfigMetadata::default()),
                },
            ],
        )));
    }

    let update = wait_for_update(watch.as_mut());
    apply_update(&update, &limits, &timeouts);
    assert!(limits.config_epoch() > limit_epoch);
    assert!(timeouts.config_epoch() > timeout_epoch);
    limit_epoch = limits.config_epoch();
    timeout_epoch = timeouts.config_epoch();

    {
        let mut events = queue.lock();
        events.push_back(ConfigDelta::Change(ChangeNotification::new(
            2,
            1_700_000_500,
            vec![ChangeEvent::Updated {
                key: idle_timeout_key(),
                value: ConfigValue::Integer(30_000, ConfigMetadata::default()),
            }],
        )));
    }

    let update = wait_for_update(watch.as_mut());
    apply_update(&update, &limits, &timeouts);
    assert!(limits.config_epoch() > limit_epoch);
    assert!(timeouts.config_epoch() > timeout_epoch);

    let final_limits = limits.snapshot();
    assert_eq!(final_limits.plan(ResourceKind::Connections).limit(), 2_048);

    let final_timeouts = timeouts.snapshot();
    assert_eq!(
        final_timeouts.request_timeout(),
        StdDuration::from_millis(8_000)
    );
    assert_eq!(
        final_timeouts.idle_timeout(),
        StdDuration::from_millis(30_000)
    );

    running.store(false, Ordering::SeqCst);
    limit_reader.join().expect("limit reader join");
    timeout_reader.join().expect("timeout reader join");
}

/// 测试配置源：提供固定初始层并通过队列输出增量事件。
struct TestSource {
    layers: Vec<ConfigurationLayer>,
    events: Arc<Mutex<VecDeque<ConfigDelta>>>,
}

impl TestSource {
    /// 创建配置源并返回与之共享事件队列的句柄。
    fn new(layers: Vec<ConfigurationLayer>) -> (Self, Arc<Mutex<VecDeque<ConfigDelta>>>) {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        (
            Self {
                layers,
                events: Arc::clone(&events),
            },
            events,
        )
    }
}

impl ConfigurationSource for TestSource {
    type Stream<'a>
        = TestStream
    where
        Self: 'a;

    fn load(&self, _profile: &ProfileId) -> Result<Vec<ConfigurationLayer>, ConfigurationError> {
        Ok(self.layers.clone())
    }

    fn watch<'a>(&'a self, _profile: &ProfileId) -> Result<Self::Stream<'a>, ConfigurationError> {
        Ok(TestStream {
            events: Arc::clone(&self.events),
        })
    }
}

/// 将队列中的 `ConfigDelta` 暴露为异步流，供 `ConfigurationWatch` 消费。
struct TestStream {
    events: Arc<Mutex<VecDeque<ConfigDelta>>>,
}

impl Stream for TestStream {
    type Item = ConfigDelta;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut guard = self.events.lock();
        if let Some(delta) = guard.pop_front() {
            Poll::Ready(Some(delta))
        } else {
            Poll::Pending
        }
    }
}

/// 阻塞直到配置流产生下一次更新，期间允许多次 `Poll::Pending`。
fn wait_for_update(
    mut watch: Pin<&mut spark_core::configuration::ConfigurationWatch<'_>>,
) -> ConfigurationUpdate {
    loop {
        match poll_stream(watch.as_mut()) {
            Poll::Ready(Some(Ok(update))) => return update,
            Poll::Ready(Some(Err(err))) => panic!("configuration stream error: {err}"),
            Poll::Ready(None) => panic!("configuration stream ended unexpectedly"),
            Poll::Pending => thread::yield_now(),
        }
    }
}

/// 将配置快照应用至限额与超时运行时对象。
fn apply_update(
    update: &ConfigurationUpdate,
    limits: &LimitRuntimeConfig,
    timeouts: &TimeoutRuntimeConfig,
) {
    match update.kind {
        ConfigurationUpdateKind::Incremental { .. } | ConfigurationUpdateKind::Refresh => {
            limits
                .update_from_configuration(&update.snapshot)
                .expect("apply limit config");
            timeouts
                .update_from_configuration(&update.snapshot)
                .expect("apply timeout config");
        }
    }
}

/// 使用无分配 waker 对流进行一次轮询。
fn poll_stream<S: Stream>(mut stream: Pin<&mut S>) -> Poll<Option<S::Item>> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    stream.as_mut().poll_next(&mut cx)
}

/// 构造永远不唤醒的 waker，用于自旋轮询配置流。
///
/// ## 意图（Why）
/// - 在热更新合约测试中，我们需要一个不会触发真实调度的 `Waker`，以便在单线程环境中自旋轮询配置流而不依赖运行时。
/// - 该函数位于 `spark-contract-tests` 内，用于验证 `spark-core` 的配置增量应用逻辑，是契约稳定性的守护用例之一。
///
/// ## 解析逻辑（How）
/// 1. 通过手工构造 `RawWakerVTable`，为 `clone`、`wake`、`wake_by_ref` 与 `drop` 提供最小实现，全部为 no-op，确保不会唤醒执行器；
/// 2. 使用 `RawWaker::new` 将空指针与上述 vtable 绑定，再交由 `Waker::from_raw` 包装成安全的 `Waker`；
/// 3. 调用方在 `poll_stream` 中借助该 waker 创建 `Context`，实现可控的轮询流程。
///
/// ## 契约定义（What）
/// - 输入：无显式参数，仅依赖内部静态 vtable，不触达外部状态；
/// - 返回：返回一个满足 `Waker` 契约的值，保证所有 vtable 操作均为幂等且不触发调度；
/// - 前置条件：调用环境必须确保无共享状态需要唤醒，否则该 waker 会导致任务饿死；
/// - 后置条件：返回的 waker 在 `RawWaker` 层面不会泄漏或重复释放资源，符合 `Waker` 安全约束。
///
/// ## 设计考量与风险（Trade-offs）
/// - 选择手工实现 vtable 而非依赖现成库，是为了避免额外依赖并精确掌控行为；
/// - 风险点在于 `unsafe` 构造 `RawWaker`，若指针或 vtable 配置错误将触发未定义行为，因此需通过审计保证实现与规范一致；
/// - 该实现只适合静态自旋场景，若未来引入真实调度器需改写为可唤醒版本。
fn noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    // SAFETY: 1) `clone`/`wake`/`wake_by_ref`/`drop` 均遵守 `RawWakerVTable` 合约，
    //         使用空指针且不读取/写入内存，确保不会解引用无效地址；
    //         2) vtable 函数之间保持一致的指针语义（均忽略输入），满足 `RawWaker` 的 clone/drop 安全前提；
    //         3) 该 waker 仅在自旋测试中使用，不会泄漏到需要真实唤醒的执行器，避免违反调度假设。
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

fn limit_key(resource: ResourceKind) -> ConfigKey {
    ConfigKey::new(
        "runtime",
        format!("limits.{}.limit", resource.as_str()),
        ConfigScope::Runtime,
        "runtime limit",
    )
}

fn request_timeout_key() -> ConfigKey {
    ConfigKey::new(
        "runtime",
        "timeouts.request_ms",
        ConfigScope::Runtime,
        "request timeout in milliseconds",
    )
}

fn idle_timeout_key() -> ConfigKey {
    ConfigKey::new(
        "runtime",
        "timeouts.idle_ms",
        ConfigScope::Runtime,
        "idle timeout in milliseconds",
    )
}
