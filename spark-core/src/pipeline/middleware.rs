use alloc::{borrow::Cow, boxed::Box, format};

use crate::{CoreError, runtime::CoreServices, sealed::Sealed};

use super::handler::{InboundHandler, OutboundHandler};

/// 描述 Middleware 或 Handler 的元数据，辅助链路编排与可观测性。
///
/// # 设计背景（Why）
/// - 借鉴 OpenTelemetry Attributes、Envoy Filter Metadata、Tower Layer Describe API，帮助平台编排系统识别组件用途。
/// - 允许在科研场景中收集构件级别的性能指标或生成链路图谱。
///
/// # 契约说明（What）
/// - `name`：组件的稳定标识，建议使用 `vendor.component` 命名。
/// - `category`：可选分类（如 `security`、`codec`、`routing`）。
/// - `summary`：人类可读描述，便于平台 UI 展示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiddlewareDescriptor {
    name: Cow<'static, str>,
    category: Cow<'static, str>,
    summary: Cow<'static, str>,
}

impl MiddlewareDescriptor {
    /// 构造新的描述对象。
    pub fn new(
        name: impl Into<Cow<'static, str>>,
        category: impl Into<Cow<'static, str>>,
        summary: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            name: name.into(),
            category: category.into(),
            summary: summary.into(),
        }
    }

    /// 构造匿名描述，常用于测试或快速原型。
    pub fn anonymous(stage: impl Into<Cow<'static, str>>) -> Self {
        let stage = stage.into();
        Self {
            name: Cow::Owned(format!("anonymous.{}", stage)),
            category: Cow::Borrowed("unspecified"),
            summary: Cow::Owned(format!("auto-generated descriptor for {}", stage)),
        }
    }

    /// 获取名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 获取类别。
    pub fn category(&self) -> &str {
        &self.category
    }

    /// 获取摘要。
    pub fn summary(&self) -> &str {
        &self.summary
    }
}

/// Middleware 装配链路时的注册接口。
///
/// # 设计背景（Why）
/// - 模仿 Express/Koa 中间件的“下一步”模式与 Tower Layer 的 `Layer::layer` 概念，将 Handler 注册过程显式化。
///
/// # 契约说明（What）
/// - `register_inbound`：以名称注册入站 Handler，顺序即执行顺序。
/// - `register_outbound`：以名称注册出站 Handler，顺序即逆向执行顺序。
/// - 实现方可在内部维护有序列表或拓扑结构（如 DAG）以支持复杂编排。
///
/// # 风险提示（Trade-offs）
/// - 若允许并行或条件执行，需在实现中扩展额外语义；此 Trait 专注于顺序链路的最小契约。
pub trait ChainBuilder: Sealed {
    /// 注册入站 Handler。
    fn register_inbound(&mut self, label: &str, handler: Box<dyn InboundHandler>);

    /// 注册出站 Handler。
    fn register_outbound(&mut self, label: &str, handler: Box<dyn OutboundHandler>);
}

/// Middleware 合约：以声明式方式将 Handler 注入 Controller。
///
/// # 设计背景（Why）
/// - 综合 Express Middleware、Envoy Filter、gRPC Interceptor、Tower Layer 的经验，通过 `configure` 方法实现可重入、可组合的链路装配。
/// - 支持科研场景：可在 `configure` 中注入测量 Handler 或模型驱动的策略。
///
/// # 契约说明（What）
/// - `descriptor`：返回组件元数据。
/// - `configure`：在给定 [`ChainBuilder`] 与 [`CoreServices`] 环境下注册 Handler。
/// - `configure` 必须是幂等操作，以支持热更新或多次装配。
///
/// # 风险提示（Trade-offs）
/// - Middleware 不应在 `configure` 中执行阻塞操作；如需异步初始化，可将任务委托给 `CoreServices` 中的执行器。
/// - 若在 `configure` 中捕获状态，需确保其线程安全并避免循环依赖。
pub trait Middleware: Send + Sync + 'static + Sealed {
    /// 返回组件元数据。
    fn descriptor(&self) -> MiddlewareDescriptor;

    /// 在链路中注册 Handler。
    fn configure(
        &self,
        chain: &mut dyn ChainBuilder,
        services: &CoreServices,
    ) -> Result<(), CoreError>;
}
