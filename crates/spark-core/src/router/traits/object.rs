use alloc::{borrow::Cow, sync::Arc, vec::Vec};

use crate::{SparkError, buffer::PipelineMessage, sealed::Sealed, service::BoxService};

use super::super::binding::{RouteBinding, RouteValidation};
use super::super::context::{RoutingContext, RoutingSnapshot};
use super::super::metadata::RouteMetadata;
use super::super::route::RouteId;
use super::generic::{RouteError, Router};
use crate::transport::{QualityOfService, SecurityMode};

/// `RouteBindingObject` 表示对象层的路由绑定结果。
///
/// # 设计初衷（Why）
/// - 为插件/脚本环境提供最小依赖的路由结果表示，避免暴露泛型 `Service` 类型；
/// - 携带与泛型 [`RouteBinding`] 等价的信息，以便在对象层继续执行 Service 调用与观测逻辑。
///
/// # 字段说明（What）
/// - `service`：封装为 [`BoxService`] 的对象层服务；
/// - `metadata`/`effective_qos`/`effective_security`：与泛型层保持一致；
/// - `id`：命中的路由 ID。
#[derive(Clone, Debug)]
pub struct RouteBindingObject {
    id: RouteId,
    service: BoxService,
    metadata: RouteMetadata,
    effective_qos: Option<QualityOfService>,
    effective_security: Option<SecurityMode>,
}

impl RouteBindingObject {
    /// 构造新的对象层绑定结果。
    pub fn new(
        id: RouteId,
        service: BoxService,
        metadata: RouteMetadata,
        effective_qos: Option<QualityOfService>,
        effective_security: Option<SecurityMode>,
    ) -> Self {
        Self {
            id,
            service,
            metadata,
            effective_qos,
            effective_security,
        }
    }

    /// 获取路由 ID。
    pub fn id(&self) -> &RouteId {
        &self.id
    }

    /// 获取对象层服务句柄。
    pub fn service(&self) -> &BoxService {
        &self.service
    }

    /// 获取路由元数据。
    pub fn metadata(&self) -> &RouteMetadata {
        &self.metadata
    }

    /// 获取生效 QoS。
    pub fn effective_qos(&self) -> Option<QualityOfService> {
        self.effective_qos
    }

    /// 获取生效安全模式。
    pub fn effective_security(&self) -> Option<&SecurityMode> {
        self.effective_security.as_ref()
    }
}

/// `RouteDecisionObject` 封装对象层的路由结果与诊断告警。
#[derive(Clone, Debug)]
pub struct RouteDecisionObject {
    binding: RouteBindingObject,
    warnings: Vec<Cow<'static, str>>,
}

impl RouteDecisionObject {
    /// 构造决策结果。
    pub fn new(binding: RouteBindingObject, warnings: Vec<Cow<'static, str>>) -> Self {
        Self { binding, warnings }
    }

    /// 访问绑定。
    pub fn binding(&self) -> &RouteBindingObject {
        &self.binding
    }

    /// 访问告警列表。
    pub fn warnings(&self) -> &[Cow<'static, str>] {
        &self.warnings
    }
}

/// 对象层路由接口，面向插件、脚本或动态加载场景。
///
/// # 设计初衷（Why）
/// - 将泛型 [`Router`] 的语义映射到 `PipelineMessage` + [`crate::service::DynService`] 的对象层世界；
/// - 支持在控制面或运行时热更新中以 trait 对象形式持有路由实现。
///
/// # 契约说明（What）
/// - `route_dyn` 接受 `PipelineMessage` 请求上下文并返回对象层决策；
/// - `snapshot`、`validate` 与泛型层完全一致；
/// - 错误类型使用 [`SparkError`] 对齐对象层其他组件。
pub trait DynRouter: Send + Sync + Sealed {
    /// 执行路由决策。
    fn route_dyn(
        &self,
        context: RoutingContext<'_, PipelineMessage>,
    ) -> Result<RouteDecisionObject, RouteError<SparkError>>;

    /// 返回当前路由快照。
    fn snapshot(&self) -> RoutingSnapshot<'_>;

    /// 预检路由配置。
    fn validate(&self, descriptor: &super::super::catalog::RouteDescriptor) -> RouteValidation;
}

/// `RouterObject` 将泛型 [`Router`] 适配为对象层 [`DynRouter`]。
///
/// # 设计初衷（Why）
/// - 遵循 T05“互转/适配器”要求：在保留泛型实现的情况下，为插件/脚本提供对象安全接口；
/// - 通过显式提供 `ServiceObject` 适配逻辑，使不同协议的请求/响应在路由层可自定义转换。
///
/// # 使用约束（Contract）
/// - 泛型路由的 `Request` 必须为 [`PipelineMessage`]，错误类型为 [`SparkError`]；
/// - 调用方需要提供 `service_adapter` 闭包，将泛型 `RouteBinding` 转换为对象层绑定（常见做法是使用
///   [`crate::service::ServiceObject`] 对泛型 Service 进行类型擦除）。
pub struct RouterObject<R, Adapter>
where
    R: Router<PipelineMessage, Error = SparkError> + Send + Sync,
    Adapter: Fn(RouteBinding<R::Service, PipelineMessage>) -> Result<RouteBindingObject, SparkError>
        + Send
        + Sync
        + 'static,
{
    inner: R,
    service_adapter: Arc<Adapter>,
}

impl<R, Adapter> RouterObject<R, Adapter>
where
    R: Router<PipelineMessage, Error = SparkError> + Send + Sync,
    Adapter: Fn(RouteBinding<R::Service, PipelineMessage>) -> Result<RouteBindingObject, SparkError>
        + Send
        + Sync
        + 'static,
{
    /// 构造新的路由对象适配器。
    pub fn new(inner: R, service_adapter: Adapter) -> Self {
        Self {
            inner,
            service_adapter: Arc::new(service_adapter),
        }
    }
}

impl<R, Adapter> DynRouter for RouterObject<R, Adapter>
where
    R: Router<PipelineMessage, Error = SparkError> + Send + Sync,
    Adapter: Fn(RouteBinding<R::Service, PipelineMessage>) -> Result<RouteBindingObject, SparkError>
        + Send
        + Sync
        + 'static,
{
    fn route_dyn(
        &self,
        context: RoutingContext<'_, PipelineMessage>,
    ) -> Result<RouteDecisionObject, RouteError<SparkError>> {
        let decision = self.inner.route(context)?;
        let (binding, warnings) = decision.into_parts();
        let binding_object = (self.service_adapter)(binding).map_err(RouteError::Internal)?;
        Ok(RouteDecisionObject::new(binding_object, warnings))
    }

    fn snapshot(&self) -> RoutingSnapshot<'_> {
        self.inner.snapshot()
    }

    fn validate(&self, descriptor: &super::super::catalog::RouteDescriptor) -> RouteValidation {
        self.inner.validate(descriptor)
    }
}
