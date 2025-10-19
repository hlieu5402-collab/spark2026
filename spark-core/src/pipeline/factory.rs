use alloc::boxed::Box;

use crate::{error::SparkError, runtime::CoreServices};

use super::controller::Controller;

/// Controller 构造工厂，负责在通道建立阶段装配完整链路。
///
/// # 设计背景（Why）
/// - 借鉴 Netty `ChannelInitializer`、Envoy Listener Filter Chain Builder、gRPC Channel Builder、Tower Stack Builder 的经验。
/// - 对于科研场景，可根据 `CoreServices` 中的实验参数动态拼装不同的 Middleware/Handler。
///
/// # 契约说明（What）
/// - **输入**：`core_services` 由宿主在启动阶段组装，包含运行时、监控、分布式等能力。
/// - **输出**：返回已经按顺序装配好的 [`Controller`] 实例。
/// - **前置条件**：调用发生在通道状态切换为 `Initialized` 后，尚未进入事件循环。
/// - **后置条件**：返回的 Controller 应立即可用，避免在热路径上再进行昂贵初始化。
///
/// # 风险提示（Trade-offs）
/// - 若 Handler 依赖外部资源，建议在工厂外部缓存，并通过 `CoreServices` 注入，以降低建链延迟。
/// - 工厂实现应确保幂等，支持在失败后重试或回滚。
pub trait ControllerFactory: Send + Sync + 'static {
    /// 构建 Controller 实例。
    fn build(&self, core_services: &CoreServices) -> Result<Box<dyn Controller>, SparkError>;
}
