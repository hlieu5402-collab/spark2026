use alloc::borrow::ToOwned;
use alloc::{borrow::Cow, boxed::Box, format, sync::Arc, vec::Vec};
use core::{fmt, time::Duration};

use crate::{
    SparkError,
    future::BoxFuture,
    observability::{OpsEvent, OwnedAttributeSet, TraceContext},
    runtime::{AsyncRuntime, CoreServices, MonotonicTimePoint},
};

use crate::contract::{CloseReason, Deadline};
use crate::pipeline::Channel;

type TriggerFn = dyn Fn(&CloseReason, Option<Deadline>) + Send + Sync + 'static;
type AwaitFn = dyn Fn() -> BoxFuture<'static, Result<(), SparkError>> + Send + Sync + 'static;
type ForceFn = dyn Fn() + Send + Sync + 'static;

/// `GracefulShutdownStatus` 表示单个关闭目标的执行结果。
///
/// # 设计初衷（Why）
/// - 宿主在停机时需要判定每个长寿命对象（Channel、Transport、Router 等）的关闭结果，以便决定是否升级为硬关闭；
/// - 在 A/B 演练或双活环境中，需要对比“全部成功”“部分失败”“被强制终止”三类情况，本枚举提供稳定语义。
///
/// # 契约说明（What）
/// - `Completed`：目标在截止时间内完成优雅关闭；
/// - `Failed`：目标显式返回 [`SparkError`]，代表调用方需进行审计或补偿；
/// - `ForcedTimeout`：在截止时间到期后仍未完成，被协调器触发硬关闭，需重点关注写缓冲是否丢失。
///
/// # 风险提示（Trade-offs）
/// - `Failed` 不会自动触发硬关闭，由调用方根据错误码决定后续动作；
/// - `ForcedTimeout` 仅表示协调器调用了硬关闭钩子，并不保证资源立即释放，宿主需结合日志/指标确认状态。
#[derive(Debug)]
#[non_exhaustive]
pub enum GracefulShutdownStatus {
    Completed,
    Failed(SparkError),
    ForcedTimeout,
}

/// `GracefulShutdownRecord` 记录单个目标的关闭摘要。
///
/// # 设计初衷（Why）
/// - 为宿主审计与单元测试提供结构化结果，避免解析日志才能获取结论；
/// - 便于在多目标停机流程中保留顺序信息，与日志、运维事件对齐。
///
/// # 契约说明（What）
/// - `label`：调用方注册目标时提供的稳定标识，用于日志与审计；
/// - `status`：对应的 [`GracefulShutdownStatus`]；
/// - `elapsed`：从发起等待到返回结果的持续时间，便于识别耗时热点。
///
/// # 风险提示（Trade-offs）
/// - `elapsed` 基于协调器读取的单调时钟，若宿主实现 `TimeDriver::sleep` 为近似值，结果也会同步偏差；
/// - 结构体设计为只读视图，不暴露可变引用，防止测试在断言时意外修改状态。
#[derive(Debug)]
pub struct GracefulShutdownRecord {
    label: Cow<'static, str>,
    status: GracefulShutdownStatus,
    elapsed: Duration,
}

impl GracefulShutdownRecord {
    /// 获取目标标识。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 获取关闭状态。
    pub fn status(&self) -> &GracefulShutdownStatus {
        &self.status
    }

    /// 获取目标关闭耗时。
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// `GracefulShutdownReport` 汇总整次停机的执行情况。
///
/// # 设计初衷（Why）
/// - 宿主需要在停机结束后得到结构化摘要，以驱动指标打点或自动化审计；
/// - 支持测试验证“全部完成 / 部分失败 / 强制关闭数量”等指标，确保契约在 CI 中可回归。
///
/// # 契约说明（What）
/// - `reason`：触发关闭的业务原因；
/// - `deadline`：整体截止时间快照；
/// - `results`：按注册顺序排列的各目标关闭记录；
/// - `forced_count()`：统计硬关闭目标数量，便于快速判断风险等级。
///
/// # 风险提示（Trade-offs）
/// - `deadline` 仅记录触发时的计划值，若宿主在流程中动态调整截止时间，需要结合日志分析；
/// - `results` 只记录协调器内部视角，不代表底层实现已经真正释放资源，必要时需结合 `OpsEvent` 与指标确认。
#[derive(Debug)]
pub struct GracefulShutdownReport {
    reason: CloseReason,
    deadline: Option<Deadline>,
    results: Vec<GracefulShutdownRecord>,
}

impl GracefulShutdownReport {
    /// 获取关闭原因。
    pub fn reason(&self) -> &CloseReason {
        &self.reason
    }

    /// 获取关闭截止计划。
    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// 访问各目标关闭结果。
    pub fn results(&self) -> &[GracefulShutdownRecord] {
        &self.results
    }

    /// 统计被强制关闭的目标数量。
    pub fn forced_count(&self) -> usize {
        self.results
            .iter()
            .filter(|record| matches!(record.status, GracefulShutdownStatus::ForcedTimeout))
            .count()
    }

    /// 统计显式失败的目标数量。
    pub fn failure_count(&self) -> usize {
        self.results
            .iter()
            .filter(|record| matches!(record.status, GracefulShutdownStatus::Failed(_)))
            .count()
    }
}

/// `GracefulShutdownTarget` 描述协调器可管理的单个关闭对象。
///
/// # 设计初衷（Why）
/// - 将“如何触发优雅关闭”“如何等待完成”“如何执行硬关闭”封装为统一结构，避免宿主在不同对象上重复编写样板代码；
/// - 支持通过自定义回调扩展到 Channel 之外的任意长寿命对象（如 Router、Transport、外部任务）。
///
/// # 契约说明（What）
/// - `label`：稳定标识，用于日志、审计、报告；
/// - `trigger_graceful`：在协调器发起关闭时执行，需尽快返回；
/// - `await_closed`：返回等待关闭完成的 Future；
/// - `force_close`：在超时或业务选择硬关闭时调用，应执行幂等操作。
///
/// # 风险提示（Trade-offs）
/// - `trigger_graceful` 在调用期间禁止阻塞，否则会拖慢整个关闭流程；
/// - 若对象不支持硬关闭，可在 `force_close` 中记录日志或保持空实现，但应在文档说明风险。
pub struct GracefulShutdownTarget {
    label: Cow<'static, str>,
    trigger_graceful: Box<TriggerFn>,
    await_closed: Box<AwaitFn>,
    force_close: Box<ForceFn>,
}

impl GracefulShutdownTarget {
    /// 基于 [`Channel`] 构造默认的关闭目标，自动调用 `close_graceful`/`closed`/`close`。
    ///
    /// # 契约说明（What）
    /// - **前置条件**：`label` 应能唯一标识通道；
    /// - **后置条件**：协调器会按照 Channel 契约触发优雅关闭并等待完成，超时时调用 `close()` 触发硬关闭。
    pub fn for_channel(label: impl Into<Cow<'static, str>>, channel: Arc<dyn Channel>) -> Self {
        let graceful_channel = Arc::clone(&channel);
        let closed_channel = Arc::clone(&channel);
        let force_channel = Arc::clone(&channel);
        Self {
            label: label.into(),
            trigger_graceful: Box::new(move |reason, deadline| {
                graceful_channel.close_graceful(reason.clone(), deadline);
            }),
            await_closed: Box::new(move || closed_channel.closed()),
            force_close: Box::new(move || {
                force_channel.close();
            }),
        }
    }

    /// 基于自定义回调构造关闭目标，适用于 Router、Service 等非 Channel 对象。
    ///
    /// # 契约说明（What）
    /// - `trigger_graceful`：接收关闭原因与截止时间，要求非阻塞；
    /// - `await_closed`：返回等待完成的 Future；
    /// - `force_close`：执行硬关闭钩子，可为 no-op。
    pub fn for_callbacks(
        label: impl Into<Cow<'static, str>>,
        trigger_graceful: impl Fn(&CloseReason, Option<Deadline>) + Send + Sync + 'static,
        await_closed: impl Fn() -> BoxFuture<'static, Result<(), SparkError>> + Send + Sync + 'static,
        force_close: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            trigger_graceful: Box::new(trigger_graceful),
            await_closed: Box::new(await_closed),
            force_close: Box::new(force_close),
        }
    }
}

impl fmt::Debug for GracefulShutdownTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GracefulShutdownTarget")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// `GracefulShutdownCoordinator` 负责串联宿主的优雅关闭流程。
///
/// # 设计初衷（Why）
/// - T19 任务要求“所有长寿对象可通知关闭并等待完成”，协调器提供统一入口管理这些对象；
/// - 将日志、运维事件、超时策略集中，实现停机流程“可观测、可审计、可测试”。
///
/// # 行为逻辑（How）
/// 1. `register_target` / `register_channel` 注册需要协同关闭的对象；
/// 2. `shutdown`：
///    - 广播 `ShutdownTriggered` 运维事件，并记录日志；
///    - 向所有目标并行发出优雅关闭通知；
///    - 逐个等待关闭完成，期间根据整体截止时间创建 `sleep` 计时；
///    - 若 Future 返回错误或超时，分别记录 `Failed` / `ForcedTimeout`；
///    - 汇总 [`GracefulShutdownReport`] 返回调用方。
///
/// # 契约说明（What）
/// - **前置条件**：`CoreServices` 内的 `runtime`、`logger`、`ops_bus` 必须有效；
/// - **输入**：`reason` 表示业务关闭原因，`deadline` 为整体截止时间（可选）；
/// - **后置条件**：所有目标至少被通知一次，超时路径会调用硬关闭钩子并输出 WARN 日志。
///
/// # 风险提示（Trade-offs）
/// - 目前按注册顺序依次等待目标完成，若需要并发等待可在后续引入 `join` 优化；
/// - 当 `deadline` 已过期时会立即触发硬关闭，确保停机流程不会卡死，但可能造成未刷写数据丢失，应结合日志评估影响。
pub struct GracefulShutdownCoordinator {
    services: CoreServices,
    targets: Vec<GracefulShutdownTarget>,
    audit_traces: Vec<TraceContext>,
}

impl GracefulShutdownCoordinator {
    /// 使用宿主运行时服务构造协调器。
    pub fn new(services: CoreServices) -> Self {
        Self {
            services,
            targets: Vec::new(),
            audit_traces: Vec::new(),
        }
    }

    /// 注册关闭目标。
    pub fn register_target(&mut self, target: GracefulShutdownTarget) {
        self.targets.push(target);
    }

    /// 以 Channel 注册关闭目标的便捷方法。
    pub fn register_channel(
        &mut self,
        label: impl Into<Cow<'static, str>>,
        channel: Arc<dyn Channel>,
    ) {
        self.register_target(GracefulShutdownTarget::for_channel(label, channel));
    }

    /// 附加审计链路追踪上下文，便于停机日志与分布式追踪关联。
    pub fn add_audit_trace(&mut self, trace: TraceContext) {
        self.audit_traces.push(trace);
    }

    /// 发起优雅关闭并等待结果，返回结构化报告。
    pub fn shutdown(
        mut self,
        reason: CloseReason,
        deadline: Option<Deadline>,
    ) -> BoxFuture<'static, GracefulShutdownReport> {
        let services = self.services.clone();
        let targets = core::mem::take(&mut self.targets);
        let traces = core::mem::take(&mut self.audit_traces);

        Box::pin(async move {
            let runtime = Arc::clone(&services.runtime);
            let logger = Arc::clone(&services.logger);
            let ops_bus = Arc::clone(&services.ops_bus);

            let now = runtime.now();
            let absolute_deadline = deadline.and_then(|d| d.instant());
            let deadline_duration = absolute_deadline
                .map(|point| point.saturating_duration_since(now))
                .unwrap_or(Duration::MAX);

            ops_bus.broadcast(OpsEvent::ShutdownTriggered {
                deadline: deadline_duration,
            });

            let mut fields = OwnedAttributeSet::new();
            fields.push_owned("shutdown.reason.code", reason.code().to_owned());
            fields.push_owned("shutdown.reason.message", reason.message().to_owned());
            fields.push_owned("shutdown.target.count", targets.len() as u64);
            fields.push_owned("shutdown.deadline.ms", deadline_duration.as_millis() as i64);
            let message = format!(
                "graceful shutdown initiated (code={}, targets={})",
                reason.code(),
                targets.len()
            );
            logger.info_with_fields(message.as_str(), fields.as_slice(), traces.first());

            for trace in traces.iter().skip(1) {
                logger.trace("shutdown trace-context attached", Some(trace));
            }

            for target in &targets {
                (target.trigger_graceful)(&reason, deadline);
            }

            let mut results = Vec::with_capacity(targets.len());
            for target in targets {
                let start = runtime.now();
                let wait_future = TimeoutFuture::new(
                    (target.await_closed)(),
                    absolute_deadline,
                    Arc::clone(&runtime),
                );

                match wait_future.await {
                    TimeoutOutcome::Completed(Ok(())) => {
                        let elapsed = runtime.now().saturating_duration_since(start);
                        results.push(GracefulShutdownRecord {
                            label: target.label,
                            status: GracefulShutdownStatus::Completed,
                            elapsed,
                        });
                    }
                    TimeoutOutcome::Completed(Err(err)) => {
                        let mut warn_fields = OwnedAttributeSet::new();
                        warn_fields
                            .push_owned("shutdown.target.label", target.label.clone().into_owned());
                        warn_fields.push_owned("shutdown.error.code", err.code());
                        logger.error_with_fields(
                            "graceful shutdown failed",
                            Some(&err),
                            warn_fields.as_slice(),
                            err.trace_context(),
                        );
                        let elapsed = runtime.now().saturating_duration_since(start);
                        results.push(GracefulShutdownRecord {
                            label: target.label,
                            status: GracefulShutdownStatus::Failed(err),
                            elapsed,
                        });
                    }
                    TimeoutOutcome::TimedOut => {
                        (target.force_close)();
                        let elapsed = runtime.now().saturating_duration_since(start);
                        let mut warn_fields = OwnedAttributeSet::new();
                        warn_fields
                            .push_owned("shutdown.target.label", target.label.clone().into_owned());
                        warn_fields.push_owned("shutdown.elapsed.ms", elapsed.as_millis() as i64);
                        logger.warn_with_fields(
                            "graceful shutdown timed out and forced",
                            warn_fields.as_slice(),
                            None,
                        );
                        results.push(GracefulShutdownRecord {
                            label: target.label,
                            status: GracefulShutdownStatus::ForcedTimeout,
                            elapsed,
                        });
                    }
                }
            }

            GracefulShutdownReport {
                reason,
                deadline,
                results,
            }
        })
    }
}

/// 内部 Future，负责在目标 Future 与超时计时之间做竞赛。
///
/// # 设计动机（Why）
/// - 避免引入额外依赖，在 `no_std + alloc` 环境下手工实现 `select` 语义；
/// - 保证协调器能够在 Future 未完成时基于运行时计时器触发硬关闭。
struct TimeoutFuture {
    future: BoxFuture<'static, Result<(), SparkError>>,
    timer: Option<BoxFuture<'static, ()>>,
}

impl TimeoutFuture {
    fn new(
        future: BoxFuture<'static, Result<(), SparkError>>,
        deadline: Option<MonotonicTimePoint>,
        runtime: Arc<dyn AsyncRuntime>,
    ) -> Self {
        let timer = deadline.map(|abs_deadline| {
            let now = runtime.now();
            let wait = abs_deadline.saturating_duration_since(now);
            if wait.is_zero() {
                let fut: BoxFuture<'static, ()> = Box::pin(async {});
                fut
            } else {
                let driver = Arc::clone(&runtime);
                let fut: BoxFuture<'static, ()> = Box::pin(async move {
                    driver.sleep(wait).await;
                });
                fut
            }
        });

        Self { future, timer }
    }
}

enum TimeoutOutcome<T> {
    Completed(T),
    TimedOut,
}

impl core::future::Future for TimeoutFuture {
    type Output = TimeoutOutcome<Result<(), SparkError>>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        if let core::task::Poll::Ready(result) = self.future.as_mut().poll(cx) {
            return core::task::Poll::Ready(TimeoutOutcome::Completed(result));
        }

        if let Some(timer) = self.timer.as_mut()
            && let core::task::Poll::Ready(()) = timer.as_mut().poll(cx)
        {
            return core::task::Poll::Ready(TimeoutOutcome::TimedOut);
        }

        core::task::Poll::Pending
    }
}
