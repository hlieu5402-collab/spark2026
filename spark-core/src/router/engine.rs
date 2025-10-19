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
/// - `ServiceUnavailable`：路由存在但绑定的 Service 当前不可用，包含路由 ID 与底层错误。
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
    ServiceUnavailable {
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
/// - `route`：
///   - 当路由表包含调用方未知的 `RouteKind::Custom`、策略引用未发布的能力或所需过滤器缺失时，
///     必须通过 [`RouteError::ServiceUnavailable`] 或 [`RouteError::Internal`] 的 `source` 映射为
///     [`crate::error::codes::ROUTER_VERSION_CONFLICT`]，提醒调用方刷新自身组件版本或协商配置。
///   - 若路由依赖的服务发现/集群元数据在网络分区、领导者丢失时不可用，应将底层错误转换为
///     [`crate::error::codes::CLUSTER_NETWORK_PARTITION`] 或 [`crate::error::codes::CLUSTER_LEADER_LOST`]，
///     以便上游执行退避与重试。
///   - 当绑定的服务池出现队列堆积、超过内部背压上限时，可暴露
///     [`crate::error::codes::CLUSTER_QUEUE_OVERFLOW`]，提示调用方切换备用路由或降级流量。
///   - 如果路由指向的实例在解析时返回陈旧视图，应以
///     [`crate::error::codes::DISCOVERY_STALE_READ`] 返回，驱动调用方刷新快照或延后读。
/// - `snapshot`：该方法不返回 `Result`，实现者应保证在网络分区等场景下退化为返回最近一次成功快照，
///   并在观测日志中补充错误码，保持与路由路径一致的可观测性。
/// - `validate`：当检测到未支持的扩展字段时，应在验证结果中附带
///   [`crate::error::codes::ROUTER_VERSION_CONFLICT`]，避免配置在控制面落地后才触发运行时错误。
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
