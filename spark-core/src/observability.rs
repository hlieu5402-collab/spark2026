use crate::{
    BoxFuture, BoxStream, Error,
    distributed::{DiscoveryEvent, MembershipEvent},
};
use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::time::Duration;

/// 链路追踪上下文。
///
/// # 设计背景（Why）
/// - 遵循 W3C Trace Context 语义，为跨服务调用提供统一标识。
/// - 在 `no_std` 环境中以定长数组存储，避免依赖 `String`。
///
/// # 契约说明（What）
/// - `trace_id`、`span_id` 分别对应 128/64 bit 标识；`sampled_flags` 以位表示采样决策。
///
/// # 风险提示（Trade-offs）
/// - 当前未包含 `tracestate`，需要时可通过 `ExtensionsMap` 扩展。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub sampled_flags: u8,
}

impl TraceContext {
    /// 创建根 Span。
    ///
    /// # 契约说明
    /// - **输入**：`sampled` 表示是否采样。
    /// - **后置条件**：返回的新上下文 `span_id` 初始化为全零，需由上层生成真实 ID。
    ///
    /// # 风险提示
    /// - 示例实现仅返回零值，用于契约测试占位；生产实现必须替换为随机或全局唯一 ID。
    pub fn new_root(sampled: bool) -> Self {
        Self {
            trace_id: [0; 16],
            span_id: [0; 8],
            sampled_flags: sampled as u8,
        }
    }

    /// 创建子 Span。
    ///
    /// # 契约说明
    /// - **输入**：父级上下文。
    /// - **后置条件**：继承 `trace_id` 与采样标志，子 `span_id` 需由实现生成。
    ///
    /// # 风险提示
    /// - 该占位实现仅将 `span_id` 设置为常量，实际运行时必须提供随机 ID。
    pub fn new_child(&self) -> Self {
        Self {
            trace_id: self.trace_id,
            span_id: [1; 8],
            sampled_flags: self.sampled_flags,
        }
    }
}

/// 对象安全的日志接口。
///
/// # 设计背景（Why）
/// - 桥接 `spark-core` 与宿主的日志实现，允许对接 `tracing`、`log` 或自研系统。
///
/// # 契约说明（What）
/// - 所有方法需线程安全且快速返回；日志格式由实现决定。
/// - `context` 提供可选追踪关联，可能为 `None`。
///
/// # 风险提示（Trade-offs）
/// - 高频调用需注意内部缓冲开销，建议实现者进行批量输出或节流。
pub trait Logger: Send + Sync + 'static {
    /// 输出 INFO 级别日志。
    fn info(&self, message: &str, context: Option<&TraceContext>);
    /// 输出 WARN 级别日志。
    fn warn(&self, message: &str, context: Option<&TraceContext>);
    /// 输出 ERROR 级别日志。
    fn error(&self, message: &str, error: Option<&dyn Error>, context: Option<&TraceContext>);
}

/// Counter 指标。
///
/// # 设计背景（Why）
/// - 表示单调递增的计数，用于统计请求量、错误量等指标。
///
/// # 契约说明（What）
/// - `labels` 必须遵循基数约束，不得包含高基数值。
///
/// # 风险提示（Trade-offs）
/// - 多线程更新时应保证内部原子性，避免丢样。
pub trait Counter: Send + Sync {
    /// 累加指标。
    fn add(&self, value: u64, labels: &[(&'static str, &'static str)]);
}

/// Gauge 指标。
///
/// # 设计背景（Why）
/// - 表达瞬时值，如连接数、队列长度。
///
/// # 契约说明（What）
/// - `set` 重置数值，`increment`/`decrement` 应在线程安全环境下操作。
///
/// # 风险提示（Trade-offs）
/// - 若底层实现使用锁，需注意高频更新可能引发竞争。
pub trait Gauge: Send + Sync {
    /// 设置数值。
    fn set(&self, value: f64, labels: &[(&'static str, &'static str)]);
    /// 增加数值。
    fn increment(&self, value: f64, labels: &[(&'static str, &'static str)]);
    /// 减少数值。
    fn decrement(&self, value: f64, labels: &[(&'static str, &'static str)]);
}

/// Histogram 指标。
///
/// # 设计背景（Why）
/// - 记录延迟、大小等分布数据，便于 P99、P999 观测。
///
/// # 契约说明（What）
/// - `record` 接受浮点值，单位需由调用方统一。
///
/// # 风险提示（Trade-offs）
/// - 高精度直方图实现可能消耗大量内存，应根据场景调整桶设置。
pub trait Histogram: Send + Sync {
    /// 记录样本。
    fn record(&self, value: f64, labels: &[(&'static str, &'static str)]);
}

/// 指标提供者。
///
/// # 设计背景（Why）
/// - 通过对象安全接口统一各类指标后端（Prometheus、StatsD）。
///
/// # 契约说明（What）
/// - 返回的指标对象应长期有效，调用方可缓存。
///
/// # 风险提示（Trade-offs）
/// - 如后端连接失败需延迟重试，避免在热路径抛出异常。
pub trait MetricsProvider: Send + Sync + 'static {
    /// 获取 Counter。
    fn counter(&self, name: &'static str) -> Box<dyn Counter>;
    /// 获取 Gauge。
    fn gauge(&self, name: &'static str) -> Box<dyn Gauge>;
    /// 获取 Histogram。
    fn histogram(&self, name: &'static str) -> Box<dyn Histogram>;
}

/// 组件健康状态。
///
/// # 设计背景（Why）
/// - 暴露运行时自检结果，供外部探针与 AIOps 使用。
///
/// # 契约说明（What）
/// - `name` 应唯一标识组件；`details` 建议提供人类可读说明。
///
/// # 风险提示（Trade-offs）
/// - 若细节信息过长，可能导致指标/日志膨胀，需适度裁剪。
#[derive(Clone, Debug)]
pub struct ComponentHealth {
    pub name: &'static str,
    pub state: HealthState,
    pub details: String,
}

/// 健康状态枚举。
///
/// # 契约说明（What）
/// - `Up` 表示健康，`Degraded` 表示部分功能异常但仍可服务，`Down` 需触发告警。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthState {
    Up,
    Down,
    Degraded,
}

/// 健康检查提供者。
///
/// # 设计背景（Why）
/// - 抽象数据库、缓存等依赖的健康探测。
///
/// # 契约说明（What）
/// - 返回的 Future 完成后应包含最新健康状态；调用频率由宿主控制。
///
/// # 风险提示（Trade-offs）
/// - 检查实现应设置超时，避免阻塞健康聚合。
pub trait HealthCheckProvider: Send + Sync + 'static {
    /// 异步执行健康检查。
    fn check_health(&self) -> BoxFuture<'static, ComponentHealth>;
}

/// 核心用户事件。
///
/// # 设计背景（Why）
/// - 统一 Handler 之间传递的规范化事件，替代 `Box<dyn Any>` 混乱语义。
///
/// # 契约说明（What）
/// - 每个事件类型需在 Handler 中有清晰的处理分支。
///
/// # 风险提示（Trade-offs）
/// - `ApplicationSpecific` 仍提供开放扩展，但使用者需自觉管理类型冲突。
#[derive(Debug, Clone)]
pub enum CoreUserEvent {
    TlsEstablished(TlsInfo),
    IdleTimeout(IdleTimeout),
    RateLimited(RateLimited),
    ConfigChanged { keys: Vec<String> },
    ApplicationSpecific(Arc<dyn core::any::Any + Send + Sync>),
}

/// TLS 信息。
///
/// # 契约说明（What）
/// - `version`、`cipher` 记录协商结果；`peer_identity` 可包含 SPIFFE ID 或 SAN。
///
/// # 风险提示（Trade-offs）
/// - 敏感信息（如证书摘要）应谨慎记录，避免泄露。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInfo {
    pub version: String,
    pub cipher: String,
    pub peer_identity: Option<String>,
}

/// 空闲超时方向。
///
/// # 契约说明（What）
/// - 区分读、写或双向空闲，帮助 Handler 采取不同策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleDirection {
    Read,
    Write,
    Both,
}

/// 空闲超时事件。
///
/// # 契约说明（What）
/// - `direction` 指示超时类型，可能用于关闭连接或发送心跳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeout {
    pub direction: IdleDirection,
}

/// 限速方向。
///
/// # 契约说明（What）
/// - 区分入站与出站限速，便于在指标中采样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDirection {
    Inbound,
    Outbound,
}

/// 限速事件。
///
/// # 契约说明（What）
/// - `direction` 表示触发方向，上层可结合速率策略调整。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    pub direction: RateDirection,
}

/// 运维事件。
///
/// # 设计背景（Why）
/// - 提供 AIOps 所需的关键事件流，包括背压、拓扑变更等。
///
/// # 契约说明（What）
/// - 各变体的字段需稳定，方便外部订阅者解析。
///
/// # 风险提示（Trade-offs）
/// - 高频事件（背压、抖动）建议总线实现去抖，否则可能导致监控风暴。
#[derive(Clone, Debug)]
pub enum OpsEvent {
    ShutdownTriggered {
        deadline: Duration,
    },
    BackpressureApplied {
        channel_id: String,
    },
    FailureClusterDetected {
        error_code: &'static str,
        count: u64,
    },
    ClusterChange(MembershipEvent),
    DiscoveryJitter(DiscoveryEvent),
    BufferLeakDetected {
        count: usize,
    },
    HealthChange(ComponentHealth),
}

/// 运维事件总线。
///
/// # 设计背景（Why）
/// - 统一广播核心运维事件，支持多观察者。
///
/// # 契约说明（What）
/// - `broadcast` 应尽量非阻塞，`subscribe` 必须提供有界缓冲并处理溢出。
///
/// # 风险提示（Trade-offs）
/// - 实现需考虑背压策略，避免订阅者过慢导致系统阻塞。
pub trait OpsEventBus: Send + Sync + 'static {
    /// 广播事件。
    fn broadcast(&self, event: OpsEvent);

    /// 订阅事件流。
    fn subscribe(&self) -> BoxStream<'static, OpsEvent>;
}
