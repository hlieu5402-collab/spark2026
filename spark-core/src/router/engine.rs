use alloc::borrow::Cow;

use crate::Error;
use crate::service::Service;

use super::binding::{RouteDecision, RouteValidation};
use super::context::RoutingContext;
use super::metadata::RouteMetadata;
use super::route::{RouteId, RoutePattern};

/// 路由错误类型，统一表达多种失败场景。
///
/// # 设计动机（Why）
/// - 借鉴 Envoy xDS/Nginx 的返回语义与学界 Policy Enforcement Point 的研究，将“找不到路由”
///   与“策略拒绝”“服务未就绪”等情况区分，方便调用方采取精确补救措施。
///
/// # 枚举项说明（What）
/// - `NotFound`：未匹配到路由，携带原始模式与属性，便于观测或回退策略使用。
/// - `PolicyDenied`：命中策略拒绝，包含拒绝原因。
/// - `ServiceNotReady`：路由存在但绑定的 Service 当前不可用，包含路由 ID 与底层错误。
/// - `Internal`：其他内部错误，直接暴露底层原因以便调试。
#[derive(Debug)]
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
    ServiceNotReady {
        id: RouteId,
        source: E,
    },
    Internal(E),
}

/// `Router` Trait 是路由子系统的核心契约。
///
/// # 设计动机（Why）
/// - 融合 Envoy 动态路由、Linkerd 服务配置、NATS subject 匹配经验，提供统一接口；
/// - 引入 `validate`、`snapshot` 等学术界常见的策略验证钩子，满足形式化验证与持续交付。
///
/// # 方法说明（What）
/// - `route`：根据上下文返回路由决策，可能包含诊断告警。
/// - `snapshot`：返回当前路由表快照，用于缓存与观测。
/// - `validate`：在控制面写入前对候选配置进行预检查。
///
/// # 契约约束
/// - **前置**：调用 `route` 前应通过 `Service::poll_ready` 检查目标服务，或在决策内部处理；
///   上下文必须保持有效生命周期。
/// - **后置**：若成功返回 [`RouteDecision`]，则保证绑定的 Service 可以被安全地消费。
///
/// # 错误契约（Error Contract）
/// - 当路由表包含调用方未知的 `RouteKind::Custom` 或绑定能力缺失时，应返回
///   [`crate::error::codes::ROUTER_VERSION_CONFLICT`]，提示调用方与控制面协商版本。
/// - 若底层服务在路由时不可达，可返回 [`crate::error::codes::CLUSTER_NODE_UNAVAILABLE`] 并附带恢复建议。
pub trait Router<Request> {
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
    fn snapshot(&self) -> super::context::RoutingSnapshot<'_>;

    /// 对路由配置进行预检。
    fn validate(&self, descriptor: &super::catalog::RouteDescriptor) -> RouteValidation;
}
