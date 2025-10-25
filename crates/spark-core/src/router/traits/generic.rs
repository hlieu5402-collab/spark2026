use alloc::borrow::Cow;

use crate::{Error, sealed::Sealed, service::Service};

use super::super::binding::{RouteDecision, RouteValidation};
use super::super::context::RoutingContext;
use super::super::metadata::RouteMetadata;
use super::super::route::{RouteId, RoutePattern};

/// 路由错误类型，统一表达多种失败场景。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy xDS、Nginx 等系统的返回语义，将“未匹配”“策略拒绝”“服务不可用”明确区分；
/// - 作为对象层 [`super::object::DynRouter`] 的泛型基线，确保双层接口在错误语义上完全一致。
///
/// # 契约说明（What）
/// - `NotFound`：未匹配到路由，附带原始模式与属性；
/// - `PolicyDenied`：命中策略拒绝，携带原因；
/// - `ServiceUnavailable`：路由存在但绑定的服务当前不可用；
/// - `Internal`：其他内部错误，直接暴露底层原因以便调试。
#[derive(Debug)]
#[non_exhaustive]
pub enum RouteError<E>
where
    E: Error,
{
    NotFound {
        pattern: RoutePattern,
        metadata: RouteMetadata,
    },
    PolicyDenied {
        reason: Cow<'static, str>,
    },
    ServiceUnavailable {
        id: RouteId,
        source: E,
    },
    Internal(E),
}

/// `Router` Trait 是路由子系统的泛型契约。
///
/// # 设计动机（Why）
/// - 融合 Envoy 动态路由、Linkerd 服务配置、NATS subject 匹配经验，为零虚分派场景提供高性能接口；
/// - 与对象层 [`super::object::DynRouter`] 配对，保证通过适配器后语义不变。
///
/// # 契约说明（What）
/// - `route`：根据上下文返回 [`RouteDecision`]，可能包含诊断告警；
/// - `snapshot`：返回当前路由表快照，用于缓存或观测；
/// - `validate`：控制面写入前的预检入口。
///
/// # 错误契约
/// - `route`/`validate` 返回的错误必须携带语义化的 [`crate::error::codes`] 错误码，以便控制面/数据面采取补救措施。
pub trait Router<Request>: Sealed {
    /// 绑定的 Service 类型。
    type Service: Service<Request>;
    /// 路由过程中产生的错误类型。
    type Error: Error;

    /// 执行路由决策。
    fn route(
        &self,
        context: RoutingContext<'_, Request>,
    ) -> Result<RouteDecision<Self::Service, Request>, RouteError<Self::Error>>;

    /// 返回当前路由快照。
    fn snapshot(&self) -> super::super::context::RoutingSnapshot<'_>;

    /// 对路由配置进行预检。
    fn validate(&self, descriptor: &super::super::catalog::RouteDescriptor) -> RouteValidation;
}
