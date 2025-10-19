use alloc::{borrow::Cow, boxed::Box, string::String, vec::Vec};

use crate::{
    buffer::PipelineMessage, error::CoreError, observability::CoreUserEvent, runtime::CoreServices,
};

use super::{
    handler::{InboundHandler, OutboundHandler},
    middleware::{ChainBuilder, Middleware},
};

/// 事件类型枚举，覆盖 Controller 在运行期间可能广播的核心事件。
///
/// # 设计背景（Why）
/// - 综合 Netty ChannelPipeline 事件、Envoy Stream Callback、Tower Service 调度生命周期，提炼统一事件集合。
/// - 为科研场景提供事件分类基准，可用于统计、追踪或模型检查。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Handler 注册信息，协助调度器与观测系统理解链路结构。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandlerRegistration {
    label: String,
    descriptor: super::middleware::MiddlewareDescriptor,
    direction: HandlerDirection,
}

impl HandlerRegistration {
    /// 构造注册信息。
    pub fn new(
        label: impl Into<String>,
        descriptor: super::middleware::MiddlewareDescriptor,
        direction: HandlerDirection,
    ) -> Self {
        Self {
            label: label.into(),
            descriptor,
            direction,
        }
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
pub trait HandlerRegistry: Send + Sync {
    /// 返回链路快照。
    fn snapshot(&self) -> Vec<HandlerRegistration>;
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
///
/// # 前置/后置条件（Contract）
/// - **前置**：实现必须保证线程安全；事件广播期间不得持有阻塞锁，避免死锁。
/// - **后置**：事件广播需保证顺序一致性（读事件顺序、写事件逆序），并在异常时触发容错流程。
///
/// # 风险提示（Trade-offs）
/// - 在高负载场景下，Middleware 装配应尽量避免动态分配，可预先缓存 Handler 实例。
/// - 若实现支持热更新，需保证 `install_middleware` 幂等且可回滚。
pub trait Controller: Send + Sync + 'static {
    /// 注册入站 Handler。
    fn register_inbound_handler(&self, label: &str, handler: Box<dyn InboundHandler>);

    /// 注册出站 Handler。
    fn register_outbound_handler(&self, label: &str, handler: Box<dyn OutboundHandler>);

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
}

impl ChainBuilder for dyn Controller {
    fn register_inbound(&mut self, label: &str, handler: Box<dyn InboundHandler>) {
        Controller::register_inbound_handler(self, label, handler);
    }

    fn register_outbound(&mut self, label: &str, handler: Box<dyn OutboundHandler>) {
        Controller::register_outbound_handler(self, label, handler);
    }
}
