use alloc::{boxed::Box, sync::Arc};

use crate::{CoreError, runtime::CoreServices, sealed::Sealed};

use crate::pipeline::Controller;

use super::generic::ControllerFactory;

/// 对象层控制器工厂接口，面向插件系统与脚本运行时。
///
/// # 设计动机（Why）
/// - 运行时注入的 Pipeline 扩展需要以 `dyn Trait` 形式注册，此接口提供统一入口。
/// - 与泛型层的 [`ControllerFactory`] 完全对齐，确保语义等价，可互相适配。
///
/// # 行为逻辑（How）
/// - `build_dyn` 接收 [`CoreServices`]，内部通常调用泛型实现再做类型擦除；
/// - 返回值为 [`ControllerHandle`]，封装 `Arc<dyn Controller>`，方便在多线程环境复用；
/// - 适配器 [`ControllerFactoryObject`] 负责将泛型实现桥接到对象层。
///
/// # 契约说明（What）
/// - **输入**：必须在 Pipeline 初始化阶段调用，遵守与泛型层一致的前置条件；
/// - **输出**：`ControllerHandle` 持有引用计数的控制器实例，调用方可安全克隆共享；
/// - **错误处理**：出现构造错误时返回 [`CoreError`]，调用方应记录并中止建链流程。
///
/// # 风险提示（Trade-offs）
/// - 相较泛型层，多一次 `Arc` 克隆与虚表跳转；若可静态确定类型仍推荐使用泛型接口。
pub trait DynControllerFactory: Send + Sync + Sealed {
    /// 构建对象层控制器句柄。
    fn build_dyn(&self, core_services: &CoreServices) -> Result<ControllerHandle, CoreError>;
}

/// `ControllerHandle` 持有对象层控制器，实现 [`Controller`] 以便在泛型上下文复用。
#[derive(Clone)]
pub struct ControllerHandle {
    inner: Arc<dyn Controller>,
}

impl ControllerHandle {
    /// 以 `Arc<dyn Controller>` 包裹现有控制器。
    pub fn new(inner: Arc<dyn Controller>) -> Self {
        Self { inner }
    }

    /// 从泛型控制器创建对象层句柄。
    pub fn from_controller<C>(controller: C) -> Self
    where
        C: Controller,
    {
        Self {
            inner: Arc::new(controller),
        }
    }

    /// 以 trait 对象形式访问内部控制器。
    pub fn as_dyn(&self) -> &(dyn Controller) {
        &*self.inner
    }
}

impl Controller for ControllerHandle {
    fn register_inbound_handler(
        &self,
        label: &str,
        handler: Box<dyn crate::pipeline::InboundHandler>,
    ) {
        self.inner.register_inbound_handler(label, handler)
    }

    fn register_inbound_handler_static(
        &self,
        label: &str,
        handler: &'static (dyn crate::pipeline::InboundHandler),
    ) {
        self.inner.register_inbound_handler_static(label, handler)
    }

    fn register_outbound_handler(
        &self,
        label: &str,
        handler: Box<dyn crate::pipeline::OutboundHandler>,
    ) {
        self.inner.register_outbound_handler(label, handler)
    }

    fn register_outbound_handler_static(
        &self,
        label: &str,
        handler: &'static (dyn crate::pipeline::OutboundHandler),
    ) {
        self.inner.register_outbound_handler_static(label, handler)
    }

    fn install_middleware(
        &self,
        middleware: &dyn crate::pipeline::Middleware,
        services: &crate::runtime::CoreServices,
    ) -> Result<(), CoreError> {
        self.inner.install_middleware(middleware, services)
    }

    fn emit_channel_activated(&self) {
        self.inner.emit_channel_activated()
    }

    fn emit_read(&self, msg: crate::buffer::PipelineMessage) {
        self.inner.emit_read(msg)
    }

    fn emit_read_completed(&self) {
        self.inner.emit_read_completed()
    }

    fn emit_writability_changed(&self, is_writable: bool) {
        self.inner.emit_writability_changed(is_writable)
    }

    fn emit_user_event(&self, event: crate::observability::CoreUserEvent) {
        self.inner.emit_user_event(event)
    }

    fn emit_exception(&self, error: CoreError) {
        self.inner.emit_exception(error)
    }

    fn emit_channel_deactivated(&self) {
        self.inner.emit_channel_deactivated()
    }

    fn registry(&self) -> &dyn crate::pipeline::HandlerRegistry {
        self.inner.registry()
    }
}

/// 将泛型控制器工厂适配为对象层实现。
pub struct ControllerFactoryObject<F>
where
    F: ControllerFactory,
{
    inner: F,
}

impl<F> ControllerFactoryObject<F>
where
    F: ControllerFactory,
{
    /// 创建新的适配器实例。
    pub fn new(inner: F) -> Self {
        Self { inner }
    }

    /// 访问内部泛型实现，便于测试或高级自定义。
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F> DynControllerFactory for ControllerFactoryObject<F>
where
    F: ControllerFactory,
{
    fn build_dyn(&self, core_services: &CoreServices) -> Result<ControllerHandle, CoreError> {
        let controller = self.inner.build(core_services)?;
        Ok(ControllerHandle::from_controller(controller))
    }
}

/// 将对象层工厂重新包装为泛型接口，便于在需要静态分派的场景复用对象实现。
pub struct DynControllerFactoryAdapter {
    inner: Arc<dyn DynControllerFactory>,
}

impl DynControllerFactoryAdapter {
    /// 以对象层工厂构造适配器。
    pub fn new(inner: Arc<dyn DynControllerFactory>) -> Self {
        Self { inner }
    }
}

impl ControllerFactory for DynControllerFactoryAdapter {
    type Controller = ControllerHandle;

    fn build(&self, core_services: &CoreServices) -> Result<Self::Controller, CoreError> {
        self.inner.build_dyn(core_services)
    }
}
