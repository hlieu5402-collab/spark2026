#![allow(unsafe_code)]
// SAFETY: HotSwap 管道上下文需要短暂脱离借用检查器来向 Handler 传递控制器引用，
// ## 意图（Why）
// - `HotSwapContext` 在遍历 Handler 时必须实现 `PipelineContext`，其生命周期需跨越 `&self`
//   的借用边界，以便在回调期间继续访问控制器；
// - 控制器需要对 Handler 链执行再次调度，因此上下文必须保留对原控制器的访问能力。
// ## 解析逻辑（How）
// 1. 构造上下文时记录 `HotSwapController` 的裸指针，确保 Handler 回调期间仍可调用原控制器；
// 2. 指针仅在受控的同步路径下解引用：调用发生在 `dispatch_inbound_from` 或通知遍历中，
//    而这些逻辑都在控制器持有 `Arc` 的生命周期内执行；
// 3. `Send`/`Sync` 的实现仅传播底层控制器的线程安全语义，其他字段均为 `Arc` 或 `Clone`，
//    不会产生额外的数据竞争。
// ## 契约（What）
// - 指针来源：始终由活跃的 `HotSwapController` 实例提供，且控制器以 `Arc` 持有，
//   确保不会在 Handler 回调期间被释放；
// - 使用前提：仅允许在 Handler 链调度时同步访问，禁止跨线程持久保存该上下文。
// ## 风险与权衡（Trade-offs）
// - 为避免在 API 层暴露生命周期参数并保持 Handler 接口简洁，我们接受受控的 `unsafe`，
//   以换取零拷贝地传递控制器引用；
// - 若未来引入跨线程异步执行器，需要重新审视该策略并可能改为 `Arc<HotSwapController>`。
use alloc::string::ToString;
use alloc::{borrow::Cow, boxed::Box, format, string::String, sync::Arc, vec::Vec};

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

use crate::{
    buffer::PipelineMessage,
    contract::{CallContext, CloseReason, Deadline},
    error::{CoreError, SparkError},
    observability::{
        CoreUserEvent, OwnedAttributeSet, TraceContext,
        metrics::contract::pipeline as pipeline_metrics,
    },
    runtime::CoreServices,
    sealed::Sealed,
};

use super::{
    channel::{Channel, WriteSignal},
    context::Context as PipelineContext,
    handler::{self, InboundHandler, OutboundHandler},
    instrument::{InstrumentedLogger, start_inbound_span},
    internal::{HandlerEpochBuffer, HotSwapRegistry},
    middleware::{ChainBuilder, Middleware},
};

/// 事件类型枚举，覆盖 Controller 在运行期间可能广播的核心事件。
///
/// # 设计背景（Why）
/// - 综合 Netty ChannelPipeline 事件、Envoy Stream Callback、Tower Service 调度生命周期，提炼统一事件集合。
/// - 为科研场景提供事件分类基准，可用于统计、追踪或模型检查。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControllerEventKind {
    /// 通道变为活跃。
    ChannelActivated,
    /// 收到一条读消息。
    ReadDispatched,
    /// 本轮读取结束。
    ReadCompleted,
    /// 可写性状态发生变化。
    WritabilityChanged,
    /// 广播用户自定义事件。
    UserEventDispatched,
    /// 捕获异常并进入容错流程。
    ExceptionRaised,
    /// 通道变为非活跃。
    ChannelDeactivated,
}

/// Controller 事件，用于对外发布遥测或审计信息。
///
/// # 契约说明（What）
/// - `kind`：事件类型。
/// - `source`：触发事件的 Handler 或 Middleware 标签。
/// - `note`：可选说明，建议用于记录关键信息（如错误分类、背压原因）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerEvent {
    kind: ControllerEventKind,
    source: Cow<'static, str>,
    note: Option<Cow<'static, str>>,
}

impl ControllerEvent {
    /// 构造新的事件。
    pub fn new(
        kind: ControllerEventKind,
        source: impl Into<Cow<'static, str>>,
        note: Option<impl Into<Cow<'static, str>>>,
    ) -> Self {
        Self {
            kind,
            source: source.into(),
            note: note.map(Into::into),
        }
    }

    /// 获取事件类型。
    pub fn kind(&self) -> ControllerEventKind {
        self.kind
    }

    /// 获取事件来源标签。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 获取事件说明。
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

/// 控制器内部使用的 Handler 句柄，配合热插拔操作标识链路节点。
///
/// # 教案式说明
/// - **意图（Why）**：在运行时热更新场景中，调用方需要一种稳定且易比较的标识来定位某个 Handler，
///   以便执行插入、替换、移除等操作。简单使用下标会受到并发修改的影响，因此引入显式句柄。
/// - **逻辑（How）**：句柄的最低位编码方向（0=Inbound，1=Outbound），高位为单调递增的序列号；
///   同时保留两个保留值作为“虚拟哨兵”，分别代表入站/出站链路的头部锚点，便于在链首插入新 Handler。
/// - **契约（What）**：句柄在控制器生命周期内唯一；比较与拷贝均为常数时间；
///   `direction()` 返回该句柄所属方向或锚点的方向提示。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControllerHandleId(u64);

impl ControllerHandleId {
    /// 入站链路的虚拟头部锚点，用于“在最前方插入”。
    pub const INBOUND_HEAD: Self = Self(0);
    /// 出站链路的虚拟头部锚点。
    pub const OUTBOUND_HEAD: Self = Self(1);

    /// 根据方向返回对应的锚点句柄，便于统一写法。
    pub fn head(direction: HandlerDirection) -> Self {
        match direction {
            HandlerDirection::Inbound => Self::INBOUND_HEAD,
            HandlerDirection::Outbound => Self::OUTBOUND_HEAD,
        }
    }

    /// 判定句柄是否为虚拟锚点。
    pub fn is_anchor(self) -> bool {
        self == Self::INBOUND_HEAD || self == Self::OUTBOUND_HEAD
    }

    /// 返回句柄所属方向；虚拟锚点也会返回其绑定方向。
    pub fn direction(self) -> HandlerDirection {
        if self == Self::INBOUND_HEAD {
            HandlerDirection::Inbound
        } else if self == Self::OUTBOUND_HEAD {
            HandlerDirection::Outbound
        } else if self.0 & 1 == 0 {
            HandlerDirection::Inbound
        } else {
            HandlerDirection::Outbound
        }
    }

    /// 将内部编码暴露给调试工具或日志系统。
    pub fn raw(self) -> u64 {
        self.0
    }

    /// 控制器内部使用的构造函数：基于方向与序列号生成唯一句柄。
    pub(crate) fn new(direction: HandlerDirection, sequence: u64) -> Self {
        let direction_bit = match direction {
            HandlerDirection::Inbound => 0u64,
            HandlerDirection::Outbound => 1u64,
        };
        Self((sequence << 1) | direction_bit)
    }
}

/// Handler 注册信息，协助调度器与观测系统理解链路结构。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerRegistration {
    handle_id: ControllerHandleId,
    label: String,
    descriptor: super::middleware::MiddlewareDescriptor,
    direction: HandlerDirection,
}

impl HandlerRegistration {
    /// 构造注册信息。
    ///
    /// # 教案式说明
    /// - **意图（Why）**：Pipeline introspection 需要同时暴露 Handler 的语义标签与可变更句柄，
    ///   以支撑后续的热插拔、可观测性标注与灰度回滚。
    /// - **逻辑（How）**：调用方传入 Handler 元数据，由注册表负责封装为只读快照；控制器在更新链路
    ///   时重新生成该快照，保证外部观察者读取到的结构与执行序一致。
    /// - **契约（What）**：`handle_id` 必须由控制器分配且在当前生命周期内唯一；`label`、`descriptor`
    ///   分别用于人类可读名称与静态描述；`direction` 指示事件流向。
    pub fn new(
        handle_id: ControllerHandleId,
        label: impl Into<String>,
        descriptor: super::middleware::MiddlewareDescriptor,
        direction: HandlerDirection,
    ) -> Self {
        Self {
            handle_id,
            label: label.into(),
            descriptor,
            direction,
        }
    }

    /// 返回 Handler 句柄，用于热插拔与回滚定位。
    pub fn handle_id(&self) -> ControllerHandleId {
        self.handle_id
    }

    /// 获取 Handler 标签。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 获取 Handler 描述。
    pub fn descriptor(&self) -> &super::middleware::MiddlewareDescriptor {
        &self.descriptor
    }

    /// 获取方向。
    pub fn direction(&self) -> HandlerDirection {
        self.direction
    }
}

/// Handler 注册方向，用于区分入站与出站链路。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HandlerDirection {
    /// 入站方向。
    Inbound,
    /// 出站方向。
    Outbound,
}

/// Handler 注册表接口，提供链路快照能力。
///
/// # 设计背景（Why）
/// - 结合 Envoy Admin API、Netty Pipeline Dump、gRPC Channelz 的经验，提供统一 introspection 能力。
///
/// # 契约说明（What）
/// - `snapshot`：返回当前链路的 Handler 列表快照，顺序与执行顺序一致。
/// - 返回值应为新分配的容器，避免外部修改内部状态。
pub trait HandlerRegistry: Send + Sync + Sealed {
    /// 返回链路快照。
    fn snapshot(&self) -> Vec<HandlerRegistration>;
}

/// 统一的 Handler 动态封装，供热插拔接口复用。
///
/// # 教案式说明
/// - **意图（Why）**：控制器在运行时接收 `Arc<dyn Handler>`，需在不暴露具体 Handler 类型的情况下
///   执行事件派发与元数据提取；该 trait 承担“方向判断 + Handler 克隆”职责。
/// - **逻辑（How）**：实现者必须返回自身的方向、静态描述，并在需要时克隆出入站或出站 Handler 的
///   `Arc`。借助 `Arc` 的引用计数语义，我们可以安全地在快照之间共享实例。
/// - **契约（What）**：若 `direction()` 返回 `Inbound`，则 `clone_inbound()` 必须返回 `Some`；
///   同理，`Outbound` 方向要求 `clone_outbound()` 返回 `Some`。允许同一 Handler 同时实现双向能力。
pub trait Handler: Send + Sync + Sealed {
    /// 返回 Handler 的主要方向，用于决定调度链路。
    fn direction(&self) -> HandlerDirection;

    /// 返回 Handler 的静态描述信息，用于构建注册表快照。
    fn descriptor(&self) -> super::middleware::MiddlewareDescriptor;

    /// 克隆出入站 Handler 引用；若不提供入站能力需返回 `None`。
    fn clone_inbound(&self) -> Option<Arc<dyn InboundHandler>> {
        None
    }

    /// 克隆出出站 Handler 引用；若不提供出站能力需返回 `None`。
    fn clone_outbound(&self) -> Option<Arc<dyn OutboundHandler>> {
        None
    }
}

/// 将入站 Handler 封装为通用 `Handler`，便于在热插拔接口中使用。
///
/// # 教案式说明
/// - **意图**：为调用方提供零拷贝转换入口，避免重复编写包装结构。
/// - **逻辑**：内部仅存储一份 `Arc<dyn InboundHandler>`，对外暴露 `Handler` 接口并在需要时克隆。
/// - **契约**：返回值可直接传递给 [`Controller::add_handler_after`] 等方法；底层 Handler 生命周期
///   由 `Arc` 管理。
pub fn handler_from_inbound(handler: Arc<dyn InboundHandler>) -> Arc<dyn Handler> {
    Arc::new(InboundHandlerSlot { inner: handler })
}

/// 将出站 Handler 封装为通用 `Handler`。
pub fn handler_from_outbound(handler: Arc<dyn OutboundHandler>) -> Arc<dyn Handler> {
    Arc::new(OutboundHandlerSlot { inner: handler })
}

struct InboundHandlerSlot {
    inner: Arc<dyn InboundHandler>,
}

impl Handler for InboundHandlerSlot {
    fn direction(&self) -> HandlerDirection {
        HandlerDirection::Inbound
    }

    fn descriptor(&self) -> super::middleware::MiddlewareDescriptor {
        self.inner.describe()
    }

    fn clone_inbound(&self) -> Option<Arc<dyn InboundHandler>> {
        Some(Arc::clone(&self.inner))
    }
}

struct OutboundHandlerSlot {
    inner: Arc<dyn OutboundHandler>,
}

impl Handler for OutboundHandlerSlot {
    fn direction(&self) -> HandlerDirection {
        HandlerDirection::Outbound
    }

    fn descriptor(&self) -> super::middleware::MiddlewareDescriptor {
        self.inner.describe()
    }

    fn clone_outbound(&self) -> Option<Arc<dyn OutboundHandler>> {
        Some(Arc::clone(&self.inner))
    }
}

/// Controller 是 Pipeline 的核心控制面，负责：
/// 1. 组织 Handler 链路（含 Middleware 装配）。
/// 2. 广播事件至 Handler。
/// 3. 暴露 introspection 与遥测接口。
///
/// # 设计背景（Why）
/// - 结合 Netty ChannelPipeline、Tower Stack、Envoy Stream FilterChain、gRPC Channel 的管理模式，确保在 Rust 环境下仍具备高并发、低延迟特性。
/// - 引入 Middleware 装配契约，支持生产级预配置与科研级试验并存。
///
/// # 契约说明（What）
/// - `register_inbound_handler` / `register_outbound_handler`：直接注册 Handler，用于动态注入或测试场景。
/// - `install_middleware`：将 Middleware 声明式配置转换为 Handler 链。
/// - `emit_*`：向链路广播事件。
/// - `registry`：返回 Handler 注册信息，支持管理面查询。
/// - `add_handler_after` / `remove_handler` / `replace_handler`：基于句柄的热插拔接口，保证运行时链路一致性。
///
/// # 线程安全与生命周期约束
/// - Trait 继承 `Send + Sync + 'static`：
///   - **原因**：控制器常由运行时线程池并发访问，并在链路初始化后长期驻留；若不要求 `'static`，将无法安全放入 `Arc<dyn Controller<HandleId = ControllerHandleId>>`。
///   - **对比**：`Context` 仅要求 `Send + Sync`，生命周期绑定单次事件回调，可由控制器控制释放时机。
/// - Handler 注册提供“拥有型 Box”与“借用型 `'static` 引用”双入口：
///   - `register_*_handler` 适合一次性构造后交由控制面托管；
///   - `register_*_handler_static` 复用全局单例或懒加载实例，避免重复分配并显式表达生命周期假设。
///
/// # 前置/后置条件（Contract）
/// - **前置**：实现必须保证线程安全；事件广播期间不得持有阻塞锁，避免死锁。
/// - **后置**：事件广播需保证顺序一致性（读事件顺序、写事件逆序），并在异常时触发容错流程。
///
/// # 风险提示（Trade-offs）
/// - 在高负载场景下，Middleware 装配应尽量避免动态分配，可预先缓存 Handler 实例。
/// - 若实现支持热更新，需保证 `install_middleware` 幂等且可回滚。
pub trait Controller: Send + Sync + 'static + Sealed {
    /// Handler 句柄类型，由具体实现指定。
    type HandleId: Copy + Eq + Send + Sync + 'static;

    /// 注册入站 Handler。
    fn register_inbound_handler(&self, label: &str, handler: Box<dyn InboundHandler>);

    /// 以 `'static` 借用方式注册入站 Handler，复用长生命周期单例。
    fn register_inbound_handler_static(&self, label: &str, handler: &'static (dyn InboundHandler)) {
        self.register_inbound_handler(label, handler::box_inbound_from_static(handler));
    }

    /// 注册出站 Handler。
    fn register_outbound_handler(&self, label: &str, handler: Box<dyn OutboundHandler>);

    /// 以 `'static` 借用方式注册出站 Handler，语义与入站版本一致。
    fn register_outbound_handler_static(
        &self,
        label: &str,
        handler: &'static (dyn OutboundHandler),
    ) {
        self.register_outbound_handler(label, handler::box_outbound_from_static(handler));
    }

    /// 通过 Middleware 批量装配 Handler。
    fn install_middleware(
        &self,
        middleware: &dyn Middleware,
        services: &CoreServices,
    ) -> Result<(), CoreError>;

    /// 将通道标记为活跃并广播事件。
    fn emit_channel_activated(&self);

    /// 向入站链路广播读取到的消息。
    fn emit_read(&self, msg: PipelineMessage);

    /// 宣告一轮读取已完成。
    fn emit_read_completed(&self);

    /// 通知可写性发生变化。
    fn emit_writability_changed(&self, is_writable: bool);

    /// 广播用户事件。
    fn emit_user_event(&self, event: CoreUserEvent);

    /// 广播异常，允许 Handler 做容错处理。
    fn emit_exception(&self, error: CoreError);

    /// 将通道标记为非活跃。
    fn emit_channel_deactivated(&self);

    /// 获取 Handler 注册表，用于链路 introspection。
    fn registry(&self) -> &dyn HandlerRegistry;

    /// 在指定句柄之后插入新的 Handler，返回新 Handler 的句柄。
    fn add_handler_after(
        &self,
        anchor: Self::HandleId,
        label: &str,
        handler: Arc<dyn Handler>,
    ) -> Self::HandleId;

    /// 根据句柄移除 Handler，若存在返回 `true`。
    fn remove_handler(&self, handle: Self::HandleId) -> bool;

    /// 使用新的实现替换指定句柄对应的 Handler。
    fn replace_handler(&self, handle: Self::HandleId, handler: Arc<dyn Handler>) -> bool;

    /// 返回当前链路的逻辑 epoch，用于观察热更新是否完成。
    fn epoch(&self) -> u64;
}

/// 内部链路节点，保存 Handler 元数据与实际回调引用。
///
/// # 教案式说明
/// - **意图（Why）**：在执行期需要一份紧凑、可原子替换的结构来描述 Handler，以便在 `ArcSwap`
///   中复制、替换并为注册表提供快照。
/// - **逻辑（How）**：`HandlerEntry` 预先缓存 `label`、`descriptor` 与方向信息，事件调度时即可直接
///   读取；`inbound` / `outbound` 字段仅在对应方向存在时为 `Some`，避免无意义的动态分派。
/// - **契约（What）**：`id` 必须对应控制器分配的句柄；若一个节点只实现入站（或出站）能力，另一侧
///   字段必须为 `None`，否则会造成事件重复分发。
struct HandlerEntry {
    id: ControllerHandleId,
    label: String,
    descriptor: super::middleware::MiddlewareDescriptor,
    direction: HandlerDirection,
    inbound: Option<Arc<dyn InboundHandler>>,
    #[allow(dead_code)]
    outbound: Option<Arc<dyn OutboundHandler>>,
}

impl HandlerEntry {
    /// 依据通用 Handler 构造节点。
    fn new(id: ControllerHandleId, label: &str, handler: Arc<dyn Handler>) -> Self {
        let direction = handler.direction();
        Self {
            id,
            label: label.to_string(),
            descriptor: handler.descriptor(),
            direction,
            inbound: handler.clone_inbound(),
            outbound: handler.clone_outbound(),
        }
    }

    /// 复用既有元数据构造替换节点。
    fn with_replacement(id: ControllerHandleId, label: String, handler: Arc<dyn Handler>) -> Self {
        let direction = handler.direction();
        Self {
            id,
            label,
            descriptor: handler.descriptor(),
            direction,
            inbound: handler.clone_inbound(),
            outbound: handler.clone_outbound(),
        }
    }

    fn handle_id(&self) -> ControllerHandleId {
        self.id
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn descriptor(&self) -> &super::middleware::MiddlewareDescriptor {
        &self.descriptor
    }

    fn direction(&self) -> HandlerDirection {
        self.direction
    }

    fn inbound(&self) -> Option<&dyn InboundHandler> {
        self.inbound.as_deref()
    }

    #[allow(dead_code)]
    fn outbound(&self) -> Option<&dyn OutboundHandler> {
        self.outbound.as_deref()
    }
}

/// 描述 HotSwapController 内部触发的变更操作类型。
///
/// # 教案式说明
/// - **意图（Why）**：集中管理 add/remove/replace 三种变更语义，
///   防止在观测路径散落裸字符串导致命名漂移或拼写错误。
/// - **逻辑（How）**：枚举值通过 [`Self::as_label`] 映射为
///   [`pipeline_metrics::OP_ADD`] 等稳定标签，供指标与日志复用。
/// - **契约（What）**：若未来扩展新的变更类型，应同步在此枚举中新增分支并更新相关告警/Runbook。
#[derive(Clone, Copy, Debug)]
enum PipelineMutationKind {
    Add,
    Remove,
    Replace,
}

impl PipelineMutationKind {
    /// 将枚举值映射为观测标签。
    fn as_label(self) -> &'static str {
        match self {
            PipelineMutationKind::Add => pipeline_metrics::OP_ADD,
            PipelineMutationKind::Remove => pipeline_metrics::OP_REMOVE,
            PipelineMutationKind::Replace => pipeline_metrics::OP_REPLACE,
        }
    }
}

/// 支持运行期热插拔的默认控制器实现。
///
/// # 教案式说明
/// - **意图（Why）**：为满足“不中断流量的链路变更”需求，控制器需在读写热路径上提供锁自由的 Handler
///   快照，并在变更时以 epoch 栅栏协调并发访问。
/// - **逻辑（How）**：
///   1. Handler 链由内部的 `HandlerEpochBuffer` 管理，内部通过
///      `ArcSwap<Vec<Arc<HandlerEntry>>>` 暴露无锁快照；
///   2. 热更新操作在 `Mutex` 保护下复制向量，插入/替换节点并调用 `HandlerEpochBuffer::store` 原子替换；
///   3. 每次变更完成后执行 `HandlerEpochBuffer::bump_epoch`，外部可据此判断新快照是否已对所有线程可见。
/// - **契约（What）**：构造时需注入通道、核心服务、调用上下文与追踪上下文；调用者必须通过 `Arc`
///   持有控制器，以保证在事件回调期间对象存活。
pub struct HotSwapController {
    channel: Arc<dyn Channel>,
    services: CoreServices,
    call_context: CallContext,
    handlers: HandlerEpochBuffer<HandlerEntry>,
    registry: HotSwapRegistry,
    sequence: AtomicU64,
    mutation: Mutex<()>,
}

/// HotSwap 控制器在观测指标中的控制器标签取值。
impl HotSwapController {
    /// 构造新的热插拔控制器。
    ///
    /// # 教案式说明
    /// - **意图**：初始化时将运行时依赖、通道以及初始上下文固定，后续热更新仅需关注 Handler 链。
    /// - **逻辑**：`HandlerEpochBuffer` 初始为空向量且 epoch 为 0；`sequence` 从 1 开始保证句柄不与
    ///   哨兵值冲突。
    /// - **契约**：`channel` 必须在控制器生命周期内保持有效；`services`、`call_context`、`trace_context`
    ///   会被克隆存储，调用方可在构造后继续使用原值。
    pub fn new(
        channel: Arc<dyn Channel>,
        services: CoreServices,
        call_context: CallContext,
    ) -> Self {
        Self {
            channel,
            services,
            call_context,
            handlers: HandlerEpochBuffer::new(),
            registry: HotSwapRegistry::new(),
            sequence: AtomicU64::new(1),
            mutation: Mutex::new(()),
        }
    }

    fn append_handler(&self, label: &str, handler: Arc<dyn Handler>) -> ControllerHandleId {
        let _guard = self.mutation.lock();
        let current = self.handlers.load();
        let mut chain: Vec<_> = current.iter().cloned().collect();
        let direction = handler.direction();
        let insert_index = chain
            .iter()
            .rposition(|entry| entry.direction() == direction)
            .map(|idx| idx + 1)
            .unwrap_or(chain.len());
        let id = self.next_handle_id(direction);
        chain.insert(
            insert_index,
            Arc::new(HandlerEntry::new(id, label, handler)),
        );
        self.commit_chain(chain, PipelineMutationKind::Add);
        id
    }

    fn next_handle_id(&self, direction: HandlerDirection) -> ControllerHandleId {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        ControllerHandleId::new(direction, sequence)
    }

    fn commit_chain(&self, chain: Vec<Arc<HandlerEntry>>, mutation: PipelineMutationKind) {
        let arc_chain = Arc::new(chain);
        let registrations = self.rebuild_registry(&arc_chain);
        self.handlers.store(Arc::clone(&arc_chain));
        self.registry.update(registrations);
        let epoch = self.handlers.bump_epoch();
        self.record_pipeline_observability(mutation, epoch);
    }

    /// 构造 Pipeline 指标与日志共用的基础标签集合。
    ///
    /// # 教案式说明
    /// - **意图（Why）**：统一控制器、Pipeline ID 等稳定维度，保证指标/日志/Runbook 均可使用相同键值。
    /// - **逻辑（How）**：以 [`OwnedAttributeSet`] 收集 `pipeline.controller`、`pipeline.id` 两个核心标签，
    ///   调用方在此基础上追加操作类型、纪元等上下文信息。
    /// - **契约（What）**：返回集合的生命周期绑定于当前调用；调用方不得缓存引用超出函数作用域。
    fn base_pipeline_attributes(&self) -> OwnedAttributeSet {
        let mut attributes = OwnedAttributeSet::new();
        attributes.push_owned(
            pipeline_metrics::ATTR_CONTROLLER,
            pipeline_metrics::CONTROLLER_HOT_SWAP,
        );
        attributes.push_owned(
            pipeline_metrics::ATTR_PIPELINE_ID,
            self.channel.id().to_string(),
        );
        attributes
    }

    /// 在 Pipeline 变更提交后记录指标与日志。
    ///
    /// # 教案式说明
    /// - **意图（Why）**：每次 Handler 变更都需要伴随可观测信号，SRE 才能通过 `spark.pipeline.epoch`
    ///   与 `spark.pipeline.mutation.total` 判断变更是否对等生效，并结合事件日志定位回滚路径。
    /// - **逻辑（How）**：
    ///   1. 构建基础标签，写入最新纪元到 Gauge；
    ///   2. 追加操作类型标签，累加变更计数；
    ///   3. 再次追加纪元字段并输出 INFO 日志 `pipeline.mutation applied epoch={n}`；
    ///      日志携带 TraceContext，便于与调用链关联。
    /// - **契约（What）**：调用方需确保仅在变更成功提交后调用；`epoch` 为逻辑时钟，单调递增。
    /// - **风险提示（Trade-offs）**：Gauge 使用 `f64` 存储纪元，若长时间运行导致值超出 2^53 需评估是否
    ///   改用高精度表示；日志字段遵循低基数要求，禁止注入请求级标识。
    fn record_pipeline_observability(&self, mutation: PipelineMutationKind, epoch: u64) {
        let gauge_attributes = self.base_pipeline_attributes();
        self.services.metrics.record_gauge_set(
            &pipeline_metrics::EPOCH,
            epoch as f64,
            gauge_attributes.as_slice(),
        );

        let mut counter_attributes = gauge_attributes.clone();
        counter_attributes.push_owned(pipeline_metrics::ATTR_MUTATION_OP, mutation.as_label());
        counter_attributes.push_owned(pipeline_metrics::ATTR_EPOCH, epoch);
        self.services.metrics.record_counter_add(
            &pipeline_metrics::MUTATION_TOTAL,
            1,
            counter_attributes.as_slice(),
        );

        let mut log_attributes = gauge_attributes;
        log_attributes.push_owned(pipeline_metrics::ATTR_MUTATION_OP, mutation.as_label());
        log_attributes.push_owned(pipeline_metrics::ATTR_EPOCH, epoch);
        let message = format!("pipeline.mutation applied epoch={epoch}");
        self.services.logger.info_with_fields(
            &message,
            log_attributes.as_slice(),
            Some(self.call_context.trace_context()),
        );
    }

    fn rebuild_registry(
        &self,
        handlers: &Arc<Vec<Arc<HandlerEntry>>>,
    ) -> Arc<Vec<HandlerRegistration>> {
        let mut entries = Vec::with_capacity(handlers.len());
        for entry in handlers.iter() {
            entries.push(HandlerRegistration::new(
                entry.handle_id(),
                entry.label().to_string(),
                entry.descriptor().clone(),
                entry.direction(),
            ));
        }
        Arc::new(entries)
    }

    fn locate_insertion_index(
        chain: &[Arc<HandlerEntry>],
        anchor: ControllerHandleId,
        direction: HandlerDirection,
    ) -> usize {
        if anchor.is_anchor() {
            chain
                .iter()
                .position(|entry| entry.direction() == direction)
                .unwrap_or(chain.len())
        } else if let Some(pos) = chain.iter().position(|entry| entry.handle_id() == anchor) {
            pos + 1
        } else {
            chain.len()
        }
    }

    fn build_context(
        &self,
        snapshot: Arc<Vec<Arc<HandlerEntry>>>,
        next_index: usize,
        trace_context: TraceContext,
    ) -> HotSwapContext {
        HotSwapContext::new(
            self,
            Arc::clone(&self.channel),
            &self.services,
            &self.call_context,
            trace_context,
            snapshot,
            next_index,
        )
    }

    fn dispatch_inbound_from(
        &self,
        snapshot: Arc<Vec<Arc<HandlerEntry>>>,
        start_index: usize,
        msg: PipelineMessage,
        parent_trace: TraceContext,
    ) {
        if let Some(index) = Self::find_next_inbound(&snapshot, start_index)
            && let Some(handler) = snapshot[index].inbound()
        {
            let span = start_inbound_span(
                snapshot[index].label(),
                snapshot[index].descriptor(),
                &parent_trace,
            );
            let ctx = self.build_context(snapshot.clone(), index + 1, span.trace_context().clone());
            handler.on_read(&ctx, msg);
            drop(span);
        }
    }

    fn find_next_inbound(snapshot: &[Arc<HandlerEntry>], start_index: usize) -> Option<usize> {
        let mut idx = start_index;
        while let Some(entry) = snapshot.get(idx) {
            if entry.inbound().is_some() {
                return Some(idx);
            }
            idx += 1;
        }
        None
    }

    fn notify_inbound<F>(&self, mut callback: F)
    where
        F: FnMut(&dyn InboundHandler, HotSwapContext),
    {
        let snapshot = self.handlers.load();
        let root_trace = self.call_context.trace_context().clone();
        for (index, entry) in snapshot.iter().enumerate() {
            if let Some(handler) = entry.inbound() {
                let ctx = self.build_context(Arc::clone(&snapshot), index + 1, root_trace.clone());
                callback(handler, ctx);
            }
        }
    }

    fn remove_by_handle(&self, handle: ControllerHandleId) -> bool {
        if handle.is_anchor() {
            return false;
        }
        let _guard = self.mutation.lock();
        let current = self.handlers.load();
        let mut chain: Vec<_> = current.iter().cloned().collect();
        if let Some(pos) = chain.iter().position(|entry| entry.handle_id() == handle) {
            chain.remove(pos);
            self.commit_chain(chain, PipelineMutationKind::Remove);
            true
        } else {
            false
        }
    }

    fn replace_by_handle(&self, handle: ControllerHandleId, handler: Arc<dyn Handler>) -> bool {
        if handle.is_anchor() {
            return false;
        }
        let _guard = self.mutation.lock();
        let current = self.handlers.load();
        let mut chain: Vec<_> = current.iter().cloned().collect();
        if let Some(pos) = chain.iter().position(|entry| entry.handle_id() == handle) {
            let existing = chain[pos].clone();
            if existing.direction() != handler.direction() {
                return false;
            }
            chain[pos] = Arc::new(HandlerEntry::with_replacement(
                handle,
                existing.label().to_string(),
                handler,
            ));
            self.commit_chain(chain, PipelineMutationKind::Replace);
            true
        } else {
            false
        }
    }
}

impl Controller for HotSwapController {
    type HandleId = ControllerHandleId;

    fn register_inbound_handler(&self, label: &str, handler: Box<dyn InboundHandler>) {
        let dyn_handler = handler_from_inbound(Arc::from(handler));
        self.append_handler(label, dyn_handler);
    }

    fn register_outbound_handler(&self, label: &str, handler: Box<dyn OutboundHandler>) {
        let dyn_handler = handler_from_outbound(Arc::from(handler));
        self.append_handler(label, dyn_handler);
    }

    fn install_middleware(
        &self,
        _middleware: &dyn Middleware,
        _services: &CoreServices,
    ) -> Result<(), CoreError> {
        Err(CoreError::new(
            "spark.pipeline.install_middleware",
            "HotSwapController::install_middleware 尚未实现：请通过 add_handler_after 装配",
        ))
    }

    fn emit_channel_activated(&self) {
        self.notify_inbound(|handler, ctx| handler.on_channel_active(&ctx));
    }

    fn emit_read(&self, msg: PipelineMessage) {
        let snapshot = self.handlers.load();
        let root_trace = self.call_context.trace_context().clone();
        self.dispatch_inbound_from(snapshot, 0, msg, root_trace);
    }

    fn emit_read_completed(&self) {
        self.notify_inbound(|handler, ctx| handler.on_read_complete(&ctx));
    }

    fn emit_writability_changed(&self, is_writable: bool) {
        self.notify_inbound(|handler, ctx| handler.on_writability_changed(&ctx, is_writable));
    }

    fn emit_user_event(&self, event: CoreUserEvent) {
        self.notify_inbound(|handler, ctx| handler.on_user_event(&ctx, clone_user_event(&event)));
    }

    fn emit_exception(&self, error: CoreError) {
        let snapshot = self.handlers.load();
        if let Some(index) = Self::find_next_inbound(&snapshot, 0)
            && let Some(handler) = snapshot[index].inbound()
        {
            let ctx = self.build_context(
                Arc::clone(&snapshot),
                index + 1,
                self.call_context.trace_context().clone(),
            );
            handler.on_exception_caught(&ctx, error);
        }
    }

    fn emit_channel_deactivated(&self) {
        self.notify_inbound(|handler, ctx| handler.on_channel_inactive(&ctx));
    }

    fn registry(&self) -> &dyn HandlerRegistry {
        &self.registry
    }

    fn add_handler_after(
        &self,
        anchor: Self::HandleId,
        label: &str,
        handler: Arc<dyn Handler>,
    ) -> Self::HandleId {
        let _guard = self.mutation.lock();
        let current = self.handlers.load();
        let mut chain: Vec<_> = current.iter().cloned().collect();
        let direction = handler.direction();
        let insert_index = Self::locate_insertion_index(&chain, anchor, direction);
        let id = self.next_handle_id(direction);
        chain.insert(
            insert_index,
            Arc::new(HandlerEntry::new(id, label, handler)),
        );
        self.commit_chain(chain, PipelineMutationKind::Add);
        id
    }

    fn remove_handler(&self, handle: Self::HandleId) -> bool {
        self.remove_by_handle(handle)
    }

    fn replace_handler(&self, handle: Self::HandleId, handler: Arc<dyn Handler>) -> bool {
        self.replace_by_handle(handle, handler)
    }

    fn epoch(&self) -> u64 {
        self.handlers.epoch()
    }
}

/// 事件调度时注入 Handler 的上下文实现。
struct HotSwapContext {
    controller: *const HotSwapController,
    channel: Arc<dyn Channel>,
    services: CoreServices,
    call_context: CallContext,
    trace_context: TraceContext,
    logger: InstrumentedLogger,
    snapshot: Arc<Vec<Arc<HandlerEntry>>>,
    next_index: AtomicUsize,
}

impl HotSwapContext {
    fn new(
        controller: &HotSwapController,
        channel: Arc<dyn Channel>,
        services: &CoreServices,
        call_context: &CallContext,
        trace_context: TraceContext,
        snapshot: Arc<Vec<Arc<HandlerEntry>>>,
        next_index: usize,
    ) -> Self {
        let logger = InstrumentedLogger::new(Arc::clone(&services.logger), trace_context.clone());
        Self {
            controller: controller as *const _,
            channel,
            services: services.clone(),
            call_context: call_context.clone(),
            trace_context,
            logger,
            snapshot,
            next_index: AtomicUsize::new(next_index),
        }
    }
}

// SAFETY: HotSwapContext 内的字段满足以下条件：
// - ## Why：为了让 Handler 在任意调度线程上复用上下文，该类型需实现 `Send`/`Sync`；
// - ## How：除 `controller` 外，其余字段均为 `Arc`、`Clone` 或原子类型，天然线程安全。
//   `controller` 指向 `HotSwapController`，其内部由 `Arc`、原子与 `Mutex` 组成，满足 Send/Sync
//   的要求；外层通过 `Arc` 持有控制器实例，确保跨线程访问不会悬垂；
// - ## What：调用者必须遵循“上下文仅在事件分发期间即时使用”的约定，不得缓存到异步任务中；
// - ## Trade-offs：保留裸指针可避免额外 `Arc` 克隆，保持调度路径轻量，代价是需要人工证明其线程安全性。
unsafe impl Send for HotSwapContext {}
unsafe impl Sync for HotSwapContext {}

impl PipelineContext for HotSwapContext {
    fn channel(&self) -> &dyn Channel {
        self.channel.as_ref()
    }

    fn controller(&self) -> &dyn Controller<HandleId = ControllerHandleId> {
        // SAFETY: `controller` 源自 `HotSwapContext::new` 中的活跃引用，
        // - ## 前提：上下文只在控制器持有 `Arc` 的生命周期内创建，指针不会被提前释放；
        // - ## 逻辑：该方法仅在 Handler 调度时同步调用，无并发写入同一控制器指针；
        // - ## 结果：返回的引用与 `HotSwapController` 原始借用等价，可安全作为只读接口使用。
        unsafe { &*self.controller }
    }

    fn executor(&self) -> &dyn crate::runtime::TaskExecutor {
        self.services.runtime() as &dyn crate::runtime::TaskExecutor
    }

    fn timer(&self) -> &dyn crate::runtime::TimeDriver {
        self.services.time_driver()
    }

    fn buffer_pool(&self) -> &dyn crate::buffer::BufferPool {
        self.services.buffer_pool.as_ref()
    }

    fn trace_context(&self) -> &TraceContext {
        &self.trace_context
    }

    fn metrics(&self) -> &dyn crate::observability::MetricsProvider {
        self.services.metrics.as_ref()
    }

    fn logger(&self) -> &dyn crate::observability::Logger {
        &self.logger
    }

    fn membership(&self) -> Option<&dyn crate::cluster::ClusterMembership> {
        self.services.membership.as_deref()
    }

    fn discovery(&self) -> Option<&dyn crate::cluster::ServiceDiscovery> {
        self.services.discovery.as_deref()
    }

    fn call_context(&self) -> &CallContext {
        &self.call_context
    }

    fn forward_read(&self, msg: PipelineMessage) {
        let index = self.next_index.fetch_add(1, Ordering::SeqCst);
        let trace = self.trace_context.clone();
        // SAFETY: 此处复用与 `controller()` 相同的裸指针：
        // - ## Why：需要将当前上下文传递给后续 Handler，实现 HotSwap 链继续前进；
        // - ## How：指针操作仅在当前线程内执行，不与其他写操作并发；
        // - ## What：`dispatch_inbound_from` 期望长期存活的控制器引用，且上下文保证 `snapshot`
        //   与 `trace` 均为 `Arc`/clone，不会悬垂；
        // - ## 风险：若未来允许异步延迟调用，需改为持有 `Arc<HotSwapController>`，当前同步模型下安全。
        unsafe {
            (*self.controller).dispatch_inbound_from(self.snapshot.clone(), index, msg, trace);
        }
    }

    fn write(&self, msg: PipelineMessage) -> Result<WriteSignal, CoreError> {
        self.channel.write(msg)
    }

    fn flush(&self) {
        self.channel.flush();
    }

    fn close_graceful(&self, reason: CloseReason, deadline: Option<Deadline>) {
        self.channel.close_graceful(reason, deadline);
    }

    fn closed(&self) -> crate::future::BoxFuture<'static, Result<(), SparkError>> {
        self.channel.closed()
    }
}

fn clone_user_event(event: &CoreUserEvent) -> CoreUserEvent {
    match event {
        CoreUserEvent::TlsEstablished(info) => CoreUserEvent::TlsEstablished(info.clone()),
        CoreUserEvent::IdleTimeout(timeout) => CoreUserEvent::IdleTimeout(*timeout),
        CoreUserEvent::RateLimited(rate) => CoreUserEvent::RateLimited(*rate),
        CoreUserEvent::ConfigChanged { keys } => {
            CoreUserEvent::ConfigChanged { keys: keys.clone() }
        }
        CoreUserEvent::ApplicationSpecific(ev) => {
            CoreUserEvent::ApplicationSpecific(Arc::clone(ev))
        }
    }
}

impl ChainBuilder for dyn Controller<HandleId = ControllerHandleId> {
    fn register_inbound(&mut self, label: &str, handler: Box<dyn InboundHandler>) {
        Controller::register_inbound_handler(self, label, handler);
    }

    fn register_inbound_static(&mut self, label: &str, handler: &'static (dyn InboundHandler)) {
        Controller::register_inbound_handler_static(self, label, handler);
    }

    fn register_outbound(&mut self, label: &str, handler: Box<dyn OutboundHandler>) {
        Controller::register_outbound_handler(self, label, handler);
    }

    fn register_outbound_static(&mut self, label: &str, handler: &'static (dyn OutboundHandler)) {
        Controller::register_outbound_handler_static(self, label, handler);
    }
}
