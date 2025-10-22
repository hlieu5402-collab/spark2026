use crate::{
    SparkError,
    context::ExecutionContext,
    observability::TraceContext,
    security::{IdentityDescriptor, SecurityPolicy},
};
use alloc::sync::Arc;
use alloc::{borrow::Cow, format, string::ToString, vec, vec::Vec};
//
// 教案级说明：为了让 Loom 在模型检查阶段能够捕获原子操作的所有调度交错，
// 当启用 `--cfg loom` 时切换到它提供的原子类型；`Arc` 保持标准实现以维持
// `Eq`/`Hash` 推导能力，避免破坏 API 契约。
#[cfg(not(loom))]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::{fmt, time::Duration};
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::runtime::MonotonicTimePoint;

/// 取消原语，统一表达跨模块的可中断性契约。
///
/// # 设计背景（Why）
/// - 评审共识要求所有长时间运行的操作都能被外部主动打断，以避免雪崩扩散或无意义的资源占用。
/// - 传统 Future/任务取消机制在 `no_std` 环境下缺乏统一接口，因此通过轻量的原子位提供最小可行解。
///
/// # 逻辑解析（How）
/// - 内部使用 [`AtomicBool`] 表达取消状态，并通过 [`Arc`] 支持多方共享。
/// - `cancel` 在首次成功设置取消位时返回 `true`，后续重复调用将返回 `false` 以提示调用方避免重复执行业务兜底。
/// - `child` 生成共享同一原子位的派生实例，便于在不同子系统传播取消信号。
///
/// # 契约说明（What）
/// - **前置条件**：构造时无需额外参数，默认处于“未取消”状态。
/// - **后置条件**：一旦调用 `cancel` 成功，`is_cancelled` 必须在全局可见，后续 `CallContext` 派生出来的任务应尽快终止或回滚。
///
/// # 设计取舍与风险（Trade-offs）
/// - 未提供回调注册接口，避免在 `no_std` 下引入调度复杂度；若需要通知机制，可在上层使用轮询或自定义事件总线。
/// - 调用者需在关键热路径自行检查 `is_cancelled`，框架不会强制终止正在执行的 Future。
#[derive(Clone, Debug)]
pub struct Cancellation {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    flag: AtomicBool,
}

impl Cancellation {
    /// 创建处于“未取消”状态的取消令牌。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationState {
                flag: AtomicBool::new(false),
            }),
        }
    }

    /// 查询当前是否已被标记取消。
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::Acquire)
    }

    /// 将当前令牌标记为取消。
    ///
    /// 返回值为 `true` 表示本次调用首次触发取消；返回 `false` 表示之前已被取消。
    pub fn cancel(&self) -> bool {
        self.inner
            .flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 派生共享同一原子位的子令牌，用于跨模块传播取消语义。
    pub fn child(&self) -> Self {
        self.clone()
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

/// 截止原语，统一描述操作的最迟完成时间。
///
/// # 设计背景（Why）
/// - 引入 `Deadline` 可以在不依赖 `std::time::Instant` 的情况下表达单调时钟的绝对超时点。
/// - 搭配 [`Cancellation`] 使用时，可在超时到期后自动标记取消，实现统一的“超时即取消”策略。
///
/// # 契约说明（What）
/// - `Deadline` 可以为空（未设置），此时代表调用方未施加硬超时限制。
/// - `with_timeout` 以当前时间点和持续时间生成新的截止点，调用方需确保 `now` 来自同一计时源。
/// - `is_expired` 基于调用时提供的当前时间判断是否超时，避免依赖壁钟。
///
/// # 风险提示（Trade-offs）
/// - 截止时间不会自动驱动取消，调用方需在检测到超时后调用 [`Cancellation::cancel`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    instant: Option<MonotonicTimePoint>,
}

impl Deadline {
    /// 创建未设置截止时间的实例。
    pub const fn none() -> Self {
        Self { instant: None }
    }

    /// 根据绝对时间点构造截止时间。
    pub fn at(instant: MonotonicTimePoint) -> Self {
        Self {
            instant: Some(instant),
        }
    }

    /// 基于当前时间点加持续时间生成截止时间。
    pub fn with_timeout(now: MonotonicTimePoint, timeout: Duration) -> Self {
        Self::at(now.saturating_add(timeout))
    }

    /// 返回内部时间点，便于与自定义调度器协作。
    pub fn instant(&self) -> Option<MonotonicTimePoint> {
        self.instant
    }

    /// 判断是否已经超时。
    pub fn is_expired(&self, now: MonotonicTimePoint) -> bool {
        match self.instant {
            Some(deadline) => now >= deadline,
            None => false,
        }
    }
}

impl Default for Deadline {
    fn default() -> Self {
        Deadline::none()
    }
}

/// 预算种类，覆盖协议解码、数据流量以及自定义资源限额。
///
/// # 设计背景（Why）
/// - 统一的预算标识便于在服务、编解码、传输层之间共享资源限额，例如统一的“解码字节数”或“并发请求”预算。
///
/// # 契约说明（What）
/// - 框架预置 `Decode` 与 `Flow` 两种常见预算；`Custom` 可用于扩展其他资源类型（例如 CPU、数据库连接数）。
/// - 自定义标识建议使用 `namespace.key` 形式，便于在日志与指标中区分来源。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BudgetKind {
    /// 协议解码预算（单位自定义，如字节、消息数）。
    Decode,
    /// 数据流量预算（单位自定义，如包数、窗口大小）。
    Flow,
    /// 自定义预算，通过稳定命名区分。
    Custom(Arc<str>),
}

impl BudgetKind {
    /// 构造自定义预算标识。
    pub fn custom(name: impl Into<Arc<str>>) -> Self {
        BudgetKind::Custom(name.into())
    }
}

/// 预算快照，用于在日志与可观测性中输出剩余额度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetSnapshot {
    kind: BudgetKind,
    remaining: u64,
    limit: u64,
}

impl BudgetSnapshot {
    /// 创建快照。
    pub fn new(kind: BudgetKind, remaining: u64, limit: u64) -> Self {
        Self {
            kind,
            remaining,
            limit,
        }
    }

    /// 获取预算类型。
    pub fn kind(&self) -> &BudgetKind {
        &self.kind
    }

    /// 查询剩余额度。
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// 查询预算上限。
    pub fn limit(&self) -> u64 {
        self.limit
    }
}

/// 预算消费决策，用于背压枚举与 Service::poll_ready 返回值。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetDecision {
    /// 预算充足，允许继续执行。
    Granted { snapshot: BudgetSnapshot },
    /// 预算已耗尽，需要上层施加背压或降级。
    Exhausted { snapshot: BudgetSnapshot },
}

impl BudgetDecision {
    /// 是否代表预算仍可用。
    pub fn is_granted(&self) -> bool {
        matches!(self, BudgetDecision::Granted { .. })
    }

    /// 快速获取预算快照。
    pub fn snapshot(&self) -> &BudgetSnapshot {
        match self {
            BudgetDecision::Granted { snapshot } | BudgetDecision::Exhausted { snapshot } => {
                snapshot
            }
        }
    }
}

/// 预算控制器，负责跨线程共享剩余额度。
///
/// # 设计背景（Why）
/// - 预算控制需要在 Service、编解码、传输等不同层级共享，因此使用 [`Arc`] + [`AtomicU64`] 保证多线程安全。
/// - 通过 `try_consume` 与 `refund` 实现幂等的租借/归还语义，便于在出错时回滚。
///
/// # 契约说明（What）
/// - `limit` 表达预算上限，`remaining` 初始等于上限。
/// - `try_consume` 会在不足时返回 `BudgetDecision::Exhausted`，并保持剩余额度不变。
/// - `refund` 在安全回滚时归还额度，结果向上取整至上限范围内。
///
/// # 风险提示（Trade-offs）
/// - 当前实现采用乐观自旋更新，适合中等竞争场景；若需严格公平或分布式预算，可在实现层封装更复杂的协调算法。
#[derive(Clone, Debug)]
pub struct Budget {
    kind: BudgetKind,
    remaining: Arc<AtomicU64>,
    limit: u64,
}

impl Budget {
    /// 使用给定上限创建预算。
    pub fn new(kind: BudgetKind, limit: u64) -> Self {
        Self {
            kind,
            remaining: Arc::new(AtomicU64::new(limit)),
            limit,
        }
    }

    /// 创建无限预算，表示不受限的资源池。
    pub fn unbounded(kind: BudgetKind) -> Self {
        Self {
            kind,
            remaining: Arc::new(AtomicU64::new(u64::MAX)),
            limit: u64::MAX,
        }
    }

    /// 返回预算类型。
    pub fn kind(&self) -> &BudgetKind {
        &self.kind
    }

    /// 查询剩余额度。
    pub fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::Acquire)
    }

    /// 获取预算上限。
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// 尝试消费指定额度。
    pub fn try_consume(&self, amount: u64) -> BudgetDecision {
        let mut current = self.remaining.load(Ordering::Acquire);
        loop {
            if current < amount {
                return BudgetDecision::Exhausted {
                    snapshot: BudgetSnapshot::new(self.kind.clone(), current, self.limit),
                };
            }
            match self.remaining.compare_exchange(
                current,
                current - amount,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return BudgetDecision::Granted {
                        snapshot: BudgetSnapshot::new(
                            self.kind.clone(),
                            current - amount,
                            self.limit,
                        ),
                    };
                }
                Err(actual) => {
                    current = actual;
                }
            }
        }
    }

    /// 归还额度，常用于异常回滚。
    pub fn refund(&self, amount: u64) {
        let mut current = self.remaining.load(Ordering::Acquire);
        loop {
            let new_value = current.saturating_add(amount).min(self.limit);
            match self.remaining.compare_exchange(
                current,
                new_value,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// 生成只读快照，用于日志或指标。
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot::new(self.kind.clone(), self.remaining(), self.limit)
    }
}

/// 安全上下文快照，承载调用者与对端的身份/策略元数据。
///
/// # 设计背景（Why）
/// - 符合“安全最小面”原则：接口默认启用安全校验，仅在显式调用 `allow_insecure` 后才允许降级。
/// - `Identity`、`Peer`、`Policy` 均以 [`Arc`] 包裹，确保在异步任务中复制成本可控且不会意外过期。
///
/// # 契约说明（What）
/// - `identity`：当前调用主体（如服务账户、人类用户）。
/// - `peer_identity`：通信对端身份（如下游服务、客户端证书）。
/// - `policy`：已生效的安全策略，用于审计与再评估。
/// - `allow_insecure`：显式标记可在当前调用中允许不安全模式（例如调试场景）。
#[derive(Clone, Debug, Default)]
pub struct SecurityContextSnapshot {
    identity: Option<Arc<IdentityDescriptor>>,
    peer_identity: Option<Arc<IdentityDescriptor>>,
    policy: Option<Arc<SecurityPolicy>>,
    allow_insecure: bool,
}

impl SecurityContextSnapshot {
    /// 设置当前调用主体。
    pub fn with_identity(mut self, identity: IdentityDescriptor) -> Self {
        self.identity = Some(Arc::new(identity));
        self
    }

    /// 设置通信对端身份。
    pub fn with_peer_identity(mut self, identity: IdentityDescriptor) -> Self {
        self.peer_identity = Some(Arc::new(identity));
        self
    }

    /// 设置生效策略。
    pub fn with_policy(mut self, policy: SecurityPolicy) -> Self {
        self.policy = Some(Arc::new(policy));
        self
    }

    /// 允许在当前调用中降级安全检查。
    pub fn allow_insecure(mut self) -> Self {
        self.allow_insecure = true;
        self
    }

    /// 获取主体身份。
    pub fn identity(&self) -> Option<&Arc<IdentityDescriptor>> {
        self.identity.as_ref()
    }

    /// 获取对端身份。
    pub fn peer_identity(&self) -> Option<&Arc<IdentityDescriptor>> {
        self.peer_identity.as_ref()
    }

    /// 获取当前策略。
    pub fn policy(&self) -> Option<&Arc<SecurityPolicy>> {
        self.policy.as_ref()
    }

    /// 是否允许降级为不安全模式。
    pub fn is_insecure_allowed(&self) -> bool {
        self.allow_insecure
    }

    /// 校验是否允许以安全模式继续执行。
    pub fn ensure_secure(&self) -> Result<(), SparkError> {
        if self.allow_insecure {
            return Ok(());
        }
        if self.identity.is_none() {
            return Err(SparkError::new(
                crate::error::codes::APP_UNAUTHORIZED,
                "缺少调用身份，禁止在安全模式下继续执行",
            ));
        }
        if self.policy.is_none() {
            return Err(SparkError::new(
                crate::error::codes::APP_UNAUTHORIZED,
                "缺少生效安全策略，禁止在安全模式下继续执行",
            ));
        }
        Ok(())
    }
}

/// 可观测性契约，声明必须上报的指标/日志字段/追踪键。
///
/// # 设计背景（Why）
/// - 专家共识要求“可观测性即契约”，因此在核心上下文中提供稳定命名，避免实现者随意命名导致数据碎片化。
///
/// # 契约说明（What）
/// - `metric_names`：稳定的指标名称集合。
/// - `log_fields`：日志必须包含的字段键。
/// - `trace_keys`：Trace/Span 传播时需要保留的上下文字段。
/// - `audit_schema`：审计事件中要求的字段列表，建议遵循不可篡改的哈希链锚点语义。
#[derive(Clone, Debug, Default)]
pub struct ObservabilityContract {
    metric_names: &'static [&'static str],
    log_fields: &'static [&'static str],
    trace_keys: &'static [&'static str],
    audit_schema: &'static [&'static str],
}

impl ObservabilityContract {
    /// 创建契约定义。
    pub const fn new(
        metric_names: &'static [&'static str],
        log_fields: &'static [&'static str],
        trace_keys: &'static [&'static str],
        audit_schema: &'static [&'static str],
    ) -> Self {
        Self {
            metric_names,
            log_fields,
            trace_keys,
            audit_schema,
        }
    }

    /// 获取必报指标名称列表。
    pub fn metric_names(&self) -> &'static [&'static str] {
        self.metric_names
    }

    /// 获取日志字段键集合。
    pub fn log_fields(&self) -> &'static [&'static str] {
        self.log_fields
    }

    /// 获取追踪上下文键列表。
    pub fn trace_keys(&self) -> &'static [&'static str] {
        self.trace_keys
    }

    /// 获取审计事件字段 Schema。
    pub fn audit_schema(&self) -> &'static [&'static str] {
        self.audit_schema
    }
}

/// 默认可观测性契约，覆盖框架内核心数据点。
pub const DEFAULT_OBSERVABILITY_CONTRACT: ObservabilityContract = ObservabilityContract::new(
    &[
        "spark.request.total",
        "spark.request.duration",
        "spark.request.inflight",
        "spark.request.errors",
        "spark.bytes.inbound",
        "spark.bytes.outbound",
        "spark.codec.encode.duration",
        "spark.codec.decode.duration",
        "spark.codec.encode.bytes",
        "spark.codec.decode.bytes",
        "spark.codec.encode.errors",
        "spark.codec.decode.errors",
        "spark.transport.connections",
        "spark.transport.connection.attempts",
        "spark.transport.connection.failures",
        "spark.transport.handshake.duration",
        "spark.transport.bytes.inbound",
        "spark.transport.bytes.outbound",
        "spark.limits.usage",
        "spark.limits.limit",
        "spark.limits.hit",
        "spark.limits.drop",
        "spark.limits.degrade",
        "spark.limits.queue.depth",
        "spark.pipeline.epoch",
        "spark.pipeline.mutation.total",
    ],
    &[
        "request.id",
        "route.id",
        "caller.identity",
        "peer.identity",
        "budget.kind",
    ],
    &["traceparent", "tracestate", "spark-budget"],
    &[
        "event_id",
        "sequence",
        "occurred_at",
        "actor_id",
        "action",
        "entity_kind",
        "entity_id",
        "state_prev_hash",
        "state_curr_hash",
        "tsa_evidence",
    ],
);

#[derive(Debug)]
struct CallContextInner {
    cancellation: Cancellation,
    deadline: Deadline,
    budgets: Vec<Budget>,
    security: SecurityContextSnapshot,
    observability: ObservabilityContract,
    trace_context: TraceContext,
}

/// 调用上下文，在核心 API 之间传递取消/截止/预算三元组与安全、可观测性信息。
///
/// # 设计背景（Why）
/// - 统一承载专家共识中强调的“取消、截止、预算”三元组，确保任意公共方法都能访问这些信息。
/// - 结合安全、可观测性契约，将身份、策略、指标命名等元信息集中，避免各组件私自约定导致割裂。
///
/// # 契约说明（What）
/// - `Cancellation`：通过 [`CallContext::cancellation`] 获取，调用方在关键路径需及时检查取消标记。
/// - `Deadline`：使用 [`CallContext::deadline`] 查询绝对超时点，可结合 [`MonotonicTimePoint`] 判断是否过期。
/// - `Budgets`：通过 [`CallContext::budget`] 或 [`CallContext::budgets`] 获取，统一协调资源限额与背压语义。
/// - `SecurityContextSnapshot`：使用 [`CallContext::security`] 检查身份策略；若未显式允许不安全模式，调用前应调用 [`SecurityContextSnapshot::ensure_secure`].
/// - `ObservabilityContract`：通过 [`CallContext::observability`] 获取指标/日志/追踪键规范。
///
/// # 风险提示（Trade-offs）
/// - `CallContext` 通过 [`Arc`] 共享，克隆成本为常数，但仍需避免在热路径上不必要的 clone。
/// - 未内置自动取消逻辑，调用方需在超时或预算耗尽时主动触发取消以避免资源泄漏。
#[derive(Clone, Debug)]
pub struct CallContext {
    inner: Arc<CallContextInner>,
}

impl CallContext {
    /// 创建上下文构建器。
    pub fn builder() -> CallContextBuilder {
        CallContextBuilder::default()
    }

    /// 获取取消原语。
    pub fn cancellation(&self) -> &Cancellation {
        &self.inner.cancellation
    }

    /// 查询截止时间。
    pub fn deadline(&self) -> Deadline {
        self.inner.deadline
    }

    /// 按种类查找预算。
    pub fn budget(&self, kind: &BudgetKind) -> Option<Budget> {
        self.inner
            .budgets
            .iter()
            .find(|b| b.kind() == kind)
            .cloned()
    }

    /// 遍历所有预算。
    pub fn budgets(&self) -> impl Iterator<Item = &Budget> {
        self.inner.budgets.iter()
    }

    /// 返回三元组只读视图，便于在热路径中读取取消/截止/预算。
    ///
    /// # 使用场景（Why）
    /// - `Service::poll_ready`、`DynService::poll_ready_dyn` 等接口仅需三元组即可决策背压；
    ///   通过该方法避免克隆整个 [`CallContext`]，降低热路径负担。
    ///
    /// # 契约说明（What）
    /// - **前置条件**：`self` 必须保持有效，且在返回的 [`ExecutionContext`] 存活期间不得释放；
    /// - **后置条件**：返回视图仅提供只读访问；预算消费仍需通过原始 [`Budget`] 调用。
    pub fn execution(&self) -> ExecutionContext<'_> {
        ExecutionContext::from(self)
    }

    /// 供 `ExecutionContext` 生成零拷贝视图的内部辅助函数。
    pub(crate) fn budgets_slice(&self) -> &[Budget] {
        &self.inner.budgets
    }

    /// 获取安全上下文。
    pub fn security(&self) -> &SecurityContextSnapshot {
        &self.inner.security
    }

    /// 获取可观测性契约。
    pub fn observability(&self) -> &ObservabilityContract {
        &self.inner.observability
    }

    /// 获取当前追踪上下文。
    pub fn trace_context(&self) -> &TraceContext {
        &self.inner.trace_context
    }
}

impl fmt::Display for CallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let deadline = match self.deadline().instant() {
            Some(instant) => format!("{:?}", instant.as_duration()),
            None => "none".to_string(),
        };
        write!(
            f,
            "CallContext{{cancelled={}, deadline={}, budgets={}}}",
            self.cancellation().is_cancelled(),
            deadline,
            self.inner.budgets.len()
        )
    }
}

/// `CallContext` 构建器，确保在创建时完成参数验证。
pub struct CallContextBuilder {
    cancellation: Cancellation,
    deadline: Deadline,
    budgets: Vec<Budget>,
    security: SecurityContextSnapshot,
    observability: ObservabilityContract,
    trace_context: TraceContext,
}

impl Default for CallContextBuilder {
    fn default() -> Self {
        Self {
            cancellation: Cancellation::new(),
            deadline: Deadline::none(),
            budgets: Vec::new(),
            security: SecurityContextSnapshot::default(),
            observability: ObservabilityContract::default(),
            trace_context: TraceContext::generate(),
        }
    }
}

impl CallContextBuilder {
    /// 设置取消原语。
    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// 设置截止时间。
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = deadline;
        self
    }

    /// 添加预算。
    pub fn add_budget(mut self, budget: Budget) -> Self {
        self.budgets.push(budget);
        self
    }

    /// 设置安全上下文。
    pub fn with_security(mut self, security: SecurityContextSnapshot) -> Self {
        self.security = security;
        self
    }

    /// 设置可观测性契约。
    pub fn with_observability(mut self, observability: ObservabilityContract) -> Self {
        self.observability = observability;
        self
    }

    /// 设置追踪上下文。
    pub fn with_trace_context(mut self, trace_context: TraceContext) -> Self {
        self.trace_context = trace_context;
        self
    }

    /// 构建上下文，若未提供预算将默认提供“无限 Flow 预算”。
    pub fn build(self) -> CallContext {
        let budgets = if self.budgets.is_empty() {
            vec![Budget::unbounded(BudgetKind::Flow)]
        } else {
            self.budgets
        };
        CallContext {
            inner: Arc::new(CallContextInner {
                cancellation: self.cancellation,
                deadline: self.deadline,
                budgets,
                security: self.security,
                observability: self.observability,
                trace_context: self.trace_context,
            }),
        }
    }
}

/// 优雅关闭理由，统一供长寿命对象（如 Channel、Transport）携带关闭原因。
///
/// # 契约说明（What）
/// - `code`：稳定的关闭码，遵循 `namespace.reason` 形式。
/// - `message`：人类可读描述，建议避免携带敏感信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseReason {
    code: Cow<'static, str>,
    message: Cow<'static, str>,
}

impl CloseReason {
    /// 构造关闭原因。
    pub fn new(code: impl Into<Cow<'static, str>>, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 获取关闭码。
    pub fn code(&self) -> &str {
        &self.code
    }

    /// 获取描述。
    pub fn message(&self) -> &str {
        &self.message
    }
}
