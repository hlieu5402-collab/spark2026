use alloc::{borrow::Cow, sync::Arc};
use core::{fmt, time::Duration};

use crate::observability::{
    MetricsProvider, OwnedAttributeSet, metrics::contract::hot_reload as contract,
};

use spin::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// 热更新切换过程的全局栅栏，确保在同一个写锁持有期间完成原子级配置替换。
///
/// # 教案级注释（Why）
/// - 限流、超时等运行时配置在热更新时需要“边界点切换”，否则读取方可能观察到撕裂状态；
/// - 通过共享的读写锁，写线程在持有写锁期间可一次性刷新同一批组件；读线程在持有读锁期间
///   则能读取到一致的快照；
/// - 该结构封装锁实现，便于未来替换为 RCU/epoch based reclamation 等机制，而不影响调用方。
///
/// # 契约说明（What）
/// - `read`：返回读锁守卫，允许多个线程并发读取快照；
/// - `write`：返回写锁守卫，在守卫释放前禁止新的读写进入，适合批量提交配置；
/// - Clone 后的实例共享同一把锁，可在不同运行时配置间传递以构造统一栅栏。
#[derive(Clone, Debug, Default)]
pub struct HotReloadFence {
    lock: Arc<RwLock<()>>,
}

impl HotReloadFence {
    /// 构造新的热更新栅栏。
    pub fn new() -> Self {
        Self {
            lock: Arc::new(RwLock::new(())),
        }
    }

    /// 进入读侧临界区，保证在守卫释放前不会被写侧打断。
    pub fn read(&self) -> HotReloadReadGuard<'_> {
        HotReloadReadGuard {
            _guard: self.lock.read(),
        }
    }

    /// 进入写侧临界区，确保在守卫释放前完成一次性配置切换。
    pub fn write(&self) -> HotReloadWriteGuard<'_> {
        HotReloadWriteGuard {
            _guard: self.lock.write(),
        }
    }
}

/// 读锁守卫的轻量包装，主要用于标记生命周期，避免直接暴露底层类型。
pub struct HotReloadReadGuard<'a> {
    _guard: RwLockReadGuard<'a, ()>,
}

impl fmt::Debug for HotReloadReadGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HotReloadReadGuard")
    }
}

/// 写锁守卫的轻量包装，配合 [`HotReloadFence`] 实现跨组件的同步提交。
pub struct HotReloadWriteGuard<'a> {
    _guard: RwLockWriteGuard<'a, ()>,
}

impl fmt::Debug for HotReloadWriteGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HotReloadWriteGuard")
    }
}

/// 热更新应用耗时的计时器封装，兼容 `std` 与 `no_std` (alloc) 构建模式。
///
/// - 在 `std` 环境下内部记录 [`std::time::Instant`]，提供真实的耗时；
/// - 在 `no_std + alloc` 下为空结构，`elapsed` 恒返回 `None`，调用方可选择性忽略直方图打点。
#[derive(Clone, Copy, Debug)]
pub struct HotReloadApplyTimer {
    #[cfg(feature = "std")]
    start: std::time::Instant,
}

impl HotReloadApplyTimer {
    /// 启动计时。
    pub fn start() -> Self {
        Self {
            #[cfg(feature = "std")]
            start: std::time::Instant::now(),
        }
    }

    /// 返回自启动以来的耗时；`no_std` 环境下返回 `None`。
    pub fn elapsed(&self) -> Option<Duration> {
        #[cfg(feature = "std")]
        {
            Some(self.start.elapsed())
        }

        #[cfg(not(feature = "std"))]
        {
            None
        }
    }
}

/// 热更新观测性的统一封装，用于在每次配置切换后上报 `config_epoch` 与应用延迟。
pub(crate) struct HotReloadObservability {
    metrics: Option<HotReloadMetrics>,
}

impl HotReloadObservability {
    /// 构造空的观测性对象，默认不打点。
    pub(crate) fn new() -> Self {
        Self { metrics: None }
    }

    /// 构造绑定指标提供者与组件标签的观测性对象。
    pub(crate) fn with_component(
        provider: Arc<dyn MetricsProvider>,
        component: Cow<'static, str>,
    ) -> Self {
        let mut attributes = OwnedAttributeSet::new();
        attributes.push_owned(contract::ATTR_COMPONENT, component.into_owned());
        Self {
            metrics: Some(HotReloadMetrics {
                provider,
                attributes,
            }),
        }
    }

    /// 记录最新纪元与可选的应用耗时。
    pub(crate) fn record(&self, epoch: u64, latency: Option<Duration>) {
        if let Some(metrics) = &self.metrics {
            metrics.record(epoch, latency);
        }
    }
}

struct HotReloadMetrics {
    provider: Arc<dyn MetricsProvider>,
    attributes: OwnedAttributeSet,
}

impl HotReloadMetrics {
    fn record(&self, epoch: u64, latency: Option<Duration>) {
        let attrs = self.attributes.as_slice();
        self.provider
            .record_gauge_set(&contract::CONFIG_EPOCH, epoch as f64, attrs);

        if let Some(latency) = latency {
            let millis = latency.as_secs_f64() * 1_000.0;
            self.provider
                .record_histogram(&contract::APPLY_LATENCY_MS, millis, attrs);
        }
    }
}
