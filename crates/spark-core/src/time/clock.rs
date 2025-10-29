// 教案级说明：本模块仅在启用 `std` Feature 后编译。
//
// - **意图 (Why)**：`Clock` 抽象的默认实现需要 `std::time::Instant`、线程调度与 `Waker`
//   机制支撑睡眠 Future；在 `no_std + alloc` 环境中无法提供这些原语，因此通过上层的
//   `#[cfg(feature = "std")] mod clock;` 控制构建。
// - **契约 (What)**：当启用 `std` Feature 时导出 [`Clock`]/[`SystemClock`]/[`MockClock`] 等具备
//   线程睡眠与原子唤醒能力的实现；关闭 `std` 时模块整体不参与编译，调用方需改用
//   [`crate::runtime::TimeDriver`] 提供的最小单调计时接口。
// - **实现提示 (How)**：父模块在 `time/mod.rs` 中使用 `#[cfg(feature = "std")]` 限定该子模
//   块；这里不再重复属性以避免 clippy 对重复属性的警告，仅在文件顶部记录约束，方便
//   阅读者理解构建条件。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use alloc::vec::Vec;

/// `Sleep` 为时钟接口返回的统一延迟 Future 类型。
///
/// # 设计意图（Why）
/// - 以 `Pin<Box<dyn Future>>` 表达异步睡眠原语，避免将具体运行时（Tokio/async-std）渗透到框架 API。
/// - 统一 Future 形态便于在测试中替换为自定义实现，同时满足对象安全需求。
///
/// # 契约说明（What）
/// - Future 完成时表示指定的持续时间已经过去；
/// - Future 必须实现 `Send + 'static` 以适配多线程调度与跨任务存活；
/// - 调用方应遵守标准 Future 契约：在返回 `Poll::Pending` 后，待 Future 状态变化时唤醒提供的 waker。
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// 抽象可注入的时钟，实现统一的“获取当前时间”与“等待指定时间”能力。
///
/// # 设计背景（Why）
/// - RetryAfter、超时与取消均依赖可靠的时间来源；若直接调用系统时钟，将导致测试难以复现。
/// - 通过 trait 注入时钟，可在生产环境使用真实时间，在测试中使用可控的虚拟时间。
///
/// # 接口约束（What）
/// - `now`：返回当前的单调时间点；
/// - `sleep`：返回一个在给定持续时间后完成的 Future；
/// - 实现者必须保证 `now` 单调递增，`sleep` 完成前至少等待所给持续时间。
///
/// # 使用指引（How）
/// - 推荐通过 `Arc<dyn Clock>` 传递给需要时间能力的组件；
/// - 测试场景可注入 [`MockClock`] 并调用其 `advance` 方法推进时间；
/// - 生产环境可使用 [`SystemClock`]，其内部委托给 Tokio 运行时。
pub trait Clock: Send + Sync + 'static {
    /// 返回当前的单调时间点。
    fn now(&self) -> Instant;

    /// 返回一个在指定持续时间后完成的睡眠 Future。
    fn sleep(&self, duration: Duration) -> Sleep;
}

/// 基于标准库线程实现的系统时钟。
///
/// # 设计动机（Why）
/// - 避免强依赖 Tokio 运行时，让 `std` 构建默认即可使用；
/// - 通过线程睡眠实现异步 `Sleep`，在不引入额外运行时的情况下满足“等待后唤醒”的契约。
///
/// # 契约说明（What）
/// - `now` 直接返回 [`Instant::now`]；
/// - `sleep` 启动后台线程执行阻塞睡眠，完成后唤醒 Future；
/// - 线程池数量取决于调用频率，调用方在高频场景下可注入自定义 [`Clock`] 以获得更高性能。
#[derive(Clone, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        Box::pin(ThreadSleep::new(duration))
    }
}

/// 线程驱动的睡眠 Future，实现最小可行的 “等待后唤醒” 契约。
///
/// # 教案式说明
/// - **意图 (Why)**：提供无需 Tokio 的默认实现，保证 `std` 构建即可使用 `Clock::sleep`。调用频率较低的控制面逻辑
///   （如 RetryAfter 节律、管理面任务）可以容忍为每次等待启动一个辅助线程。
/// - **实现逻辑 (How)**：构造时立即启动一个后台线程执行阻塞睡眠；线程醒来后将完成位标记为真并唤醒登记的 waker；
///   Future 在 `poll` 时若尚未完成，则记录最新 waker 并返回 `Poll::Pending`。
/// - **契约 (What)**：Future 在完成前保持 `Pending`，完成后返回 `Ready(())`；若 Future 在等待过程中被丢弃，后台线程最终会自
///   行退出且不会尝试唤醒已释放的 waker。
/// - **权衡 (Trade-offs)**：该实现为简化依赖而牺牲了一定性能；对于高频低延迟场景，建议注入自定义时钟（例如基于 Tokio 或专
///   用定时轮）以减少线程创建开销。
struct ThreadSleep {
    state: Arc<ThreadSleepState>,
}

impl ThreadSleep {
    fn new(duration: Duration) -> Self {
        Self {
            state: ThreadSleepState::spawn(duration),
        }
    }
}

impl Future for ThreadSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.is_completed() {
            Poll::Ready(())
        } else {
            self.state.register_waker(cx.waker());
            if self.state.is_completed() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
}

struct ThreadSleepState {
    completed: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl ThreadSleepState {
    fn spawn(duration: Duration) -> Arc<Self> {
        let state = Arc::new(Self {
            completed: AtomicBool::new(false),
            waker: Mutex::new(None),
        });
        let thread_state = Arc::clone(&state);
        thread::spawn(move || {
            thread::sleep(duration);
            thread_state.finish();
        });
        state
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn register_waker(&self, waker: &Waker) {
        let mut slot = self.waker.lock().expect("thread-sleep waker lock poisoned");
        *slot = Some(waker.clone());
    }

    fn finish(&self) {
        self.completed.store(true, Ordering::Release);
        let maybe_waker = self
            .waker
            .lock()
            .expect("thread-sleep waker lock poisoned")
            .take();
        if let Some(waker) = maybe_waker {
            waker.wake();
        }
    }
}

/// 虚拟时钟：通过手动推进时间以在测试中复现确定性的唤醒序列。
///
/// # 设计动机（Why）
/// - `RetryAfter`、超时与取消逻辑需要在 CI 中保证 100% 可重复；
/// - 虚拟时钟允许测试显式控制时间推进，并立即触发对应 waker，避免真实时间的抖动。
///
/// # 行为概览（How）
/// - 内部维护 `elapsed`（自构造起的偏移量）与待触发的睡眠列表；
/// - `advance` 增加偏移量并唤醒到期的睡眠 Future；
/// - `sleep` 创建绑定虚拟时钟的 Future，遵循标准 `Poll` 契约。
///
/// # 契约说明（What）
/// - `advance` 可以多次调用，偏移量单调增加；
/// - `sleep` 返回的 Future 在未到期前必须返回 `Poll::Pending`，并在 waker 被唤醒后重新轮询；
/// - 若 Future 被提前 Drop，将从调度队列中移除并标记为取消。
#[derive(Clone, Debug)]
pub struct MockClock {
    inner: Arc<MockClockInner>,
}

impl MockClock {
    /// 创建起始时间为当前系统时间的虚拟时钟。
    ///
    /// # 行为说明（How）
    /// - 记录构造时的 [`Instant`] 作为参考基准；
    /// - 后续通过 `elapsed` 表示自基准起累积的虚拟时间。
    pub fn new() -> Self {
        Self::with_start(Instant::now())
    }

    /// 以指定起始时间构造虚拟时钟，便于在测试中固定初始偏移。
    pub fn with_start(origin: Instant) -> Self {
        let state = ClockState {
            origin,
            elapsed: Duration::from_secs(0),
            sleepers: Vec::new(),
            next_id: 0,
        };
        Self {
            inner: Arc::new(MockClockInner {
                state: Mutex::new(state),
            }),
        }
    }

    /// 手动推进虚拟时钟。
    ///
    /// # 契约说明（What）
    /// - `delta` 必须为非负持续时间；
    /// - 函数返回后，所有到期的睡眠 Future 将立即被唤醒；
    /// - 唤醒顺序按照睡眠登记顺序稳定排序，确保测试序列可复现。
    pub fn advance(&self, delta: Duration) {
        if delta.is_zero() {
            return;
        }

        let mut to_wake = Vec::new();
        let mut guard = self
            .inner
            .state
            .lock()
            .expect("mock-clock state lock poisoned");
        guard.elapsed = guard.elapsed.saturating_add(delta);
        let elapsed = guard.elapsed;
        guard.sleepers.retain(|entry| {
            if entry.cancelled.load(Ordering::SeqCst) {
                return false;
            }
            if elapsed >= entry.deadline {
                entry.completed.store(true, Ordering::SeqCst);
                if let Some(waker) = entry.take_waker() {
                    to_wake.push(waker);
                }
                false
            } else {
                true
            }
        });
        drop(guard);

        for waker in to_wake {
            waker.wake();
        }
    }

    /// 返回自起始时间以来的虚拟时间偏移。
    pub fn elapsed(&self) -> Duration {
        self.inner
            .state
            .lock()
            .expect("mock-clock state lock poisoned")
            .elapsed
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MockClock {
    fn now(&self) -> Instant {
        let guard = self
            .inner
            .state
            .lock()
            .expect("mock-clock state lock poisoned");
        guard.origin + guard.elapsed
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        let state = {
            let mut guard = self
                .inner
                .state
                .lock()
                .expect("mock-clock state lock poisoned");
            let deadline = guard.elapsed.saturating_add(duration);
            let id = guard.next_id;
            guard.next_id += 1;
            let state = Arc::new(SleepState::new(id, deadline));
            guard.sleepers.push(Arc::clone(&state));
            state
        };

        Box::pin(MockSleep {
            inner: Arc::clone(&self.inner),
            state,
        })
    }
}

#[derive(Debug)]
struct MockClockInner {
    state: Mutex<ClockState>,
}

#[derive(Debug)]
struct ClockState {
    origin: Instant,
    elapsed: Duration,
    sleepers: Vec<Arc<SleepState>>,
    next_id: usize,
}

#[derive(Debug)]
struct SleepState {
    id: usize,
    deadline: Duration,
    waker: Mutex<Option<Waker>>,
    completed: AtomicBool,
    cancelled: AtomicBool,
}

impl SleepState {
    fn new(id: usize, deadline: Duration) -> Self {
        Self {
            id,
            deadline,
            waker: Mutex::new(None),
            completed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }

    fn take_waker(&self) -> Option<Waker> {
        self.waker.lock().expect("sleep-state waker lock").take()
    }

    fn store_waker(&self, waker: &Waker) {
        let mut guard = self.waker.lock().expect("sleep-state waker lock");
        if guard
            .as_ref()
            .is_some_and(|existing| existing.will_wake(waker))
        {
            return;
        }
        *guard = Some(waker.clone());
    }
}

struct MockSleep {
    inner: Arc<MockClockInner>,
    state: Arc<SleepState>,
}

impl Future for MockSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.cancelled.load(Ordering::SeqCst)
            || self.state.completed.load(Ordering::SeqCst)
        {
            return Poll::Ready(());
        }

        let elapsed = self
            .inner
            .state
            .lock()
            .expect("mock-clock state lock poisoned")
            .elapsed;

        if elapsed >= self.state.deadline {
            self.state.completed.store(true, Ordering::SeqCst);
            return Poll::Ready(());
        }

        self.state.store_waker(cx.waker());
        Poll::Pending
    }
}

impl Drop for MockSleep {
    fn drop(&mut self) {
        if !self.state.completed.load(Ordering::SeqCst) {
            self.state.cancelled.store(true, Ordering::SeqCst);
            self.state.take_waker();
            if let Ok(mut guard) = self.inner.state.lock() {
                guard.sleepers.retain(|entry| entry.id != self.state.id);
            }
        }
    }
}
