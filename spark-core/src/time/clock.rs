#![cfg(feature = "std")]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
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

/// 基于 Tokio 实现的系统时钟。
///
/// # 设计动机（Why）
/// - 在生产环境下复用 Tokio 时间驱动，避免重复造轮子；
/// - 通过 trait 封装，隐藏 Tokio 依赖，保持框架 API 简洁。
///
/// # 契约说明（What）
/// - `now` 直接返回 [`Instant::now`]；
/// - `sleep` 使用 [`tokio::time::sleep`]，由 Tokio 调度；
/// - 需要在 Tokio 运行时上下文中使用，否则 Future 将无法前进。
#[derive(Clone, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) -> Sleep {
        // Tokio `sleep` 返回 `!Unpin` Future，统一包裹为 `Sleep` 类型。
        Box::pin(tokio::time::sleep(duration))
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
