use crate::{
    SparkError,
    buffer::{BufferPool, PipelineMessage},
    cluster::{ClusterMembership, ServiceDiscovery},
    observability::{CoreUserEvent, Logger, MetricsProvider, TraceContext},
    runtime::{CoreServices, TaskExecutor, TimeDriver},
    transport::TransportSocketAddr,
};
use alloc::boxed::Box;
use core::{
    any::{Any, TypeId},
    time::Duration,
};

/// `ChannelState` 描述通道生命周期的有限状态机。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChannelState {
    /// 初始态：资源已分配但尚未进入活跃读写。
    Initialized,
    /// 活跃态：底层连接建立完毕，可进行全双工 I/O。
    Active,
    /// 析构态：执行优雅关闭，拒绝新入站读但允许冲刷剩余写。
    Draining,
    /// 终止态：所有资源释放，后续事件必须被忽略。
    Closed,
}

/// `ExtensionsMap` 为 Handler 提供线程安全的扩展存储。
///
/// # 设计背景（Why）
/// - Handler 需要在生命周期内携带跨模块共享的状态（如 TLS 信息、限流策略）。
/// - 通过隐藏底层并发容器，我们允许宿主根据运行环境选择最优实现（`dashmap`、`RwLock` 等）。
///
/// # 契约说明（What）
/// - 插入、读取、删除均以 `TypeId` 为键，鼓励调用者封装新类型避免碰撞。
/// - 所有存入对象必须满足 `'static + Send + Sync`，确保跨线程安全。
///
/// # 风险提示（Trade-offs）
/// - 读取返回引用，其生命周期受 `&self` 约束；若需要长期持有，请改用 `remove` 并手动管理。
pub trait ExtensionsMap: Send + Sync {
    /// 插入指定类型 ID 对应的扩展数据。
    fn insert(&self, key: TypeId, value: Box<dyn Any + Send + Sync>);

    /// 获取扩展数据的共享引用。
    fn get<'a>(&'a self, key: &TypeId) -> Option<&'a (dyn Any + Send + Sync + 'static)>;

    /// 移除扩展数据，返回拥有所有权的 Box。
    fn remove(&self, key: &TypeId) -> Option<Box<dyn Any + Send + Sync>>;

    /// 判断扩展是否存在。
    fn contains_key(&self, key: &TypeId) -> bool;

    /// 清空所有扩展。
    fn clear(&self);
}

/// `Channel` 抽象单个 I/O 连接的生命周期。
///
/// # 设计背景（Why）
/// - 将底层传输（TCP、UDP、QUIC）统一为一套操作接口，使 Handler 不必关心细节差异。
/// - 通过状态机枚举与背压信号，促进上层协议遵循相同的资源管理节奏。
///
/// # 契约说明（What）
/// - 各方法除 `write` 可能失败外，其余应尽量为幂等操作，便于在异常或重试场景调用。
/// - `close_graceful`、`close` 必须在内部更新状态并触发对应事件，避免 Handler 捕获不到生命周期变化。
///
/// # 风险提示（Trade-offs）
/// - 某些传输并不天然支持可写性检测，此时实现者需通过内部阈值模拟，必要时引入虚假的 `BackpressureApplied` 以保护系统。
pub trait Channel: Send + Sync + 'static {
    /// 返回便于日志关联的 Channel 唯一 ID。
    fn id(&self) -> &str;

    /// 获取当前状态机状态。
    fn state(&self) -> ChannelState;

    /// 指示当前是否可写。
    fn is_writable(&self) -> bool;

    /// 返回关联的 Pipeline 引用。
    fn pipeline(&self) -> &dyn Pipeline;

    /// 返回扩展属性映射。
    fn extensions(&self) -> &dyn ExtensionsMap;

    /// 获取对端地址。
    fn peer_addr(&self) -> Option<TransportSocketAddr>;

    /// 获取本地地址。
    fn local_addr(&self) -> Option<TransportSocketAddr>;

    /// 触发优雅关闭，允许在截止前冲刷缓冲。
    fn close_graceful(&self, deadline: Option<Duration>);

    /// 立即终止连接。
    fn close(&self);

    /// 向通道写入消息，返回背压信号。
    fn write(&self, msg: PipelineMessage) -> Result<WriteSignal, SparkError>;

    /// 刷新底层缓冲。
    fn flush(&self);
}

/// `WriteSignal` 表达通道写入时的背压反馈。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSignal {
    /// 消息进入缓冲，尚待刷出。
    Accepted,
    /// 消息已经落盘或发送。
    AcceptedAndFlushed,
    /// 触发背压，调用方应重试或降速。
    BackpressureApplied,
}

/// `Pipeline` 负责组织 Handler 链路并广播事件。
///
/// # 设计背景（Why）
/// - 借鉴 Netty/Tokio-io 模式，通过事件链路实现协议栈的松耦合组合。
/// - Pipeline 自身只定义事件广播，具体 Handler 顺序与组合由工厂决定。
///
/// # 契约说明（What）
/// - `fire_*` 方法均应保持非阻塞；如 Handler 需要执行耗时操作，应委托给运行时。
/// - `fire_exception_caught` 的默认实现应关闭通道，确保异常不会导致资源泄漏。
///
/// # 风险提示（Trade-offs）
/// - 若 Pipeline 内部采用锁保护 Handler 链，需注意避免在 Handler 调用外部同步代码时导致死锁。
pub trait Pipeline: Send + Sync + 'static {
    /// 通道转为活跃态时触发。
    fn fire_channel_active(&self);

    /// 通道收到读取事件。
    fn fire_read(&self, msg: PipelineMessage);

    /// 一批读取完成。
    fn fire_read_complete(&self);

    /// 通道可写性发生改变。
    fn fire_writability_changed(&self, is_writable: bool);

    /// 派发规范化用户事件。
    fn fire_user_event(&self, event: CoreUserEvent);

    /// 通道出现异常。
    fn fire_exception_caught(&self, error: SparkError);

    /// 通道变为非活跃状态。
    fn fire_channel_inactive(&self);
}

/// `Context` 是 Handler 与运行时交互的入口。
///
/// # 设计背景（Why）
/// - Handler 自身保持无状态，所有外部资源均通过 Context 提供，符合依赖倒置原则。
/// - Context 充当事件传播的网关，Handler 通过它继续向前/向后传递消息。
///
/// # 契约说明（What）
/// - `membership`、`discovery` 可能为空；Handler 必须处理缺省分布式能力。
/// - 写路径需遵循背压信号：若返回 `BackpressureApplied`，Handler 应暂停发送或调度重试。
///
/// # 风险提示（Trade-offs）
/// - Context 实现通常携带引用计数指针；在循环调用时注意避免产生长链引用而无法回收。
pub trait Context: Send + Sync {
    /// 当前通道引用。
    fn channel(&self) -> &dyn Channel;

    /// 当前管道引用。
    fn pipeline(&self) -> &dyn Pipeline;

    /// 执行器引用。
    fn executor(&self) -> &dyn TaskExecutor;

    /// 计时器引用。
    fn timer(&self) -> &dyn TimeDriver;

    /// 缓冲池访问接口。
    ///
    /// # 契约说明
    /// - 返回值必须实现 [`BufferPool`]，供 Handler 在编解码过程中租借/归还缓冲。
    /// - 调用方不得缓存引用超过事件回调生命周期，避免破坏池的自适应调度。
    fn buffer_pool(&self) -> &dyn BufferPool;

    /// 链路追踪上下文。
    fn trace_context(&self) -> &TraceContext;

    /// 指标提供者。
    fn metrics(&self) -> &dyn MetricsProvider;

    /// 日志器。
    fn logger(&self) -> &dyn Logger;

    /// 集群成员能力。
    fn membership(&self) -> Option<&dyn ClusterMembership>;

    /// 服务发现能力。
    fn discovery(&self) -> Option<&dyn ServiceDiscovery>;

    /// 继续向后传播读事件。
    fn fire_read(&self, msg: PipelineMessage);

    /// 写消息。
    fn write(&self, msg: PipelineMessage) -> Result<WriteSignal, SparkError>;

    /// 刷新缓冲。
    fn flush(&self);

    /// 优雅关闭。
    fn close_graceful(&self, deadline: Option<Duration>);
}

/// `InboundHandler` 处理正向事件。
///
/// # 设计背景（Why）
/// - 正向事件负责从底层字节到上层业务的逐层加工（解码、认证、路由）。
/// - 将接口拆分为多个回调，便于 Handler 实现精细的背压与空闲控制。
///
/// # 契约说明（What）
/// - 回调需保持线程安全；若涉及共享状态，应通过 `ExtensionsMap` 或外部同步原语处理。
/// - `on_exception_caught` 默认应进行降级或关闭通道，以免异常扩散。
///
/// # 风险提示（Trade-offs）
/// - Handler 内禁止阻塞调用运行时，避免拖慢事件循环；长任务请交由 `ctx.executor().spawn`。
pub trait InboundHandler: Send + Sync + 'static {
    /// 通道活跃时调用。
    fn on_channel_active(&self, ctx: &dyn Context);

    /// 处理读到的消息。
    fn on_read(&self, ctx: &dyn Context, msg: PipelineMessage);

    /// 一批读取完成。
    fn on_read_complete(&self, ctx: &dyn Context);

    /// 可写性变化。
    fn on_writability_changed(&self, ctx: &dyn Context, is_writable: bool);

    /// 用户事件。
    fn on_user_event(&self, ctx: &dyn Context, event: CoreUserEvent);

    /// 异常处理。
    fn on_exception_caught(&self, ctx: &dyn Context, error: SparkError);

    /// 通道不再活跃。
    fn on_channel_inactive(&self, ctx: &dyn Context);
}

/// `OutboundHandler` 处理反向写事件。
///
/// # 设计背景（Why）
/// - 写链中通常包含编码、加密、分片等步骤，需要按逆序组合处理。
///
/// # 契约说明（What）
/// - `on_write` 必须遵循背压信号语义，将结果向前一层返回。
/// - `on_close_graceful` 应负责在截止时间前冲刷缓冲，并根据需要触发后续 Handler 的关闭操作。
///
/// # 风险提示（Trade-offs）
/// - 若实现内部持有缓冲池，应考虑在异常路径释放资源，以免造成泄漏。
pub trait OutboundHandler: Send + Sync + 'static {
    /// 写入消息。
    fn on_write(&self, ctx: &dyn Context, msg: PipelineMessage) -> Result<WriteSignal, SparkError>;

    /// 刷新写缓冲。
    fn on_flush(&self, ctx: &dyn Context) -> Result<(), SparkError>;

    /// 优雅关闭。
    fn on_close_graceful(
        &self,
        ctx: &dyn Context,
        deadline: Option<Duration>,
    ) -> Result<(), SparkError>;
}

/// `PipelineFactory` 在通道建立阶段构造完整的 Handler 链。
///
/// # 设计背景（Why）
/// - 宿主在 `accept/connect` 成功后需要即时准备好协议栈，避免在热路径中再次分配或查找共享依赖。
/// - 通过统一工厂接口，我们可以注入 `CoreServices`，确保 Handler 拥有一致的运行时能力。
///
/// # 契约说明（What）
/// - **输入**：`core_services` 由宿主在启动阶段组装，包含运行时、监控、分布式等能力。
/// - **输出**：必须返回一个已经按预期顺序排列的 `Pipeline` 实例。
/// - **前置条件**：调用发生在通道状态切换为 `Initialized` 后，尚未进入事件循环。
/// - **后置条件**：返回的 Pipeline 将立即用于事件传播，因此应避免昂贵的初始化逻辑。
///
/// # 风险提示（Trade-offs）
/// - 若 Handler 依赖额外外部资源，建议在工厂外部预先缓存，并通过 `CoreServices` 或扩展字段注入，以降低延迟抖动。
pub trait PipelineFactory: Send + Sync + 'static {
    /// 构建管道实例。
    fn build(&self, core_services: &CoreServices) -> Result<Box<dyn Pipeline>, SparkError>;
}
