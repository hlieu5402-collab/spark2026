use crate::{CoreError, runtime::CoreServices, sealed::Sealed};

use crate::pipeline::{Controller, controller::ControllerHandleId};

/// 泛型层的控制器工厂合约，提供零虚分派的装配路径。
///
/// # 设计初衷（Why）
/// - 将 Pipeline 控制面构造过程泛型化，允许在内建实现中完全依赖静态分派，避免热路径额外的虚表开销。
/// - 同时保留对象层作为插件/脚本入口，满足“同一实现双层语义等价”的 T05 目标。
///
/// # 行为逻辑（How）
/// 1. 宿主在通道初始化阶段调用 [`build`](ControllerFactory::build)。
/// 2. 泛型实现根据 [`CoreServices`] 装配 Handler/Middleware，返回具体的 [`Controller`] 实例。
/// 3. 返回值由调用方直接持有，不经 `Box<dyn Controller>` 擦除，因此后续调用保持零虚分派。
///
/// # 契约说明（What）
/// - **关联类型**：[`Controller`](ControllerFactory::Controller) 必须实现 Pipeline 的控制器契约，且满足 `Send + Sync + 'static`。
/// - **输入参数**：[`CoreServices`] 聚合运行时依赖（调度、计时、监控等），实现者应在构造阶段借助该依赖注入资源。
/// - **前置条件**：调用发生在通道进入事件循环之前，避免在热路径执行昂贵初始化。
/// - **后置条件**：成功返回的控制器应立即可用，且具备线程安全性。
///
/// # 设计考量（Trade-offs）
/// - 泛型层避免 `Box` 分配，但意味着调用方需要在编译期获取具体类型；在需要运行时组合的场景，可改用对象层。
/// - 若控制器构造失败，必须返回结构化的 [`CoreError`]，以便宿主记录诊断信息。
pub trait ControllerFactory: Send + Sync + 'static + Sealed {
    /// 泛型层构造出的控制器类型。
    type Controller: Controller<HandleId = ControllerHandleId>;

    /// 构建控制器并装配完整 Pipeline 链路。
    fn build(&self, core_services: &CoreServices) -> Result<Self::Controller, CoreError>;
}
