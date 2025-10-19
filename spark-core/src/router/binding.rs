use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use crate::service::Service;
use crate::transport::{QualityOfService, SecurityMode};

use super::metadata::RouteMetadata;
use super::route::RouteId;

/// `RouteBinding` 表示路由器为一次请求挑选出的具体服务实例。
///
/// # 设计动机（Why）
/// - 结合 gRPC LB policy、Envoy Cluster 绑定的经验，将“匹配到的路由 ID”和“可执行的 Service”
///   绑定，确保后续中间件能够基于相同语义继续执行（如熔断、观测采样）。
/// - 结构中携带元数据，便于重构后的流水线共享统一上下文。
///
/// # 字段说明（What）
/// - `id`：已解析的路由标识。
/// - `service`：对应的业务处理器，实现 [`Service`] 契约。
/// - `metadata`：合并后的属性，包含静态配置与请求动态属性。
/// - `effective_qos`：决策后的 QoS 级别，可能来自意图或路由默认值。
/// - `effective_security`：选定的安全模式。
///
/// # 前置/后置条件
/// - **前置**：路由器在返回结构前需确保 `service` 已准备好（通过 `poll_ready` 语义或延迟检查）。
/// - **后置**：调用方在持有 `RouteBinding` 后，应负责驱动 `service.call` 并消费其生命周期。
pub struct RouteBinding<S, Request>
where
    S: Service<Request>,
{
    id: RouteId,
    service: S,
    metadata: RouteMetadata,
    effective_qos: Option<QualityOfService>,
    effective_security: Option<SecurityMode>,
    request: PhantomData<Request>,
}

impl<S, Request> RouteBinding<S, Request>
where
    S: Service<Request>,
{
    /// 构造绑定结果。
    pub fn new(
        id: RouteId,
        service: S,
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
            request: PhantomData,
        }
    }

    /// 获取路由 ID。
    pub fn id(&self) -> &RouteId {
        &self.id
    }

    /// 获取 Service 引用。
    pub fn service(&self) -> &S {
        &self.service
    }

    /// 获取 Service 可变引用。
    pub fn service_mut(&mut self) -> &mut S {
        &mut self.service
    }

    /// 获取合并元数据。
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

/// `RouteDecision` 封装一次路由决策的结果与说明。
///
/// # 设计动机（Why）
/// - 参考 AWS App Mesh/Envoy 的 `RouteAction`，在绑定结构外提供额外的诊断信息与可观测记录，
///   以便控制面审计与测试。
///
/// # 字段说明（What）
/// - `binding`：实际绑定结果。
/// - `warnings`：非致命告警，如使用默认 QoS、降级安全模式等。
pub struct RouteDecision<S, Request>
where
    S: Service<Request>,
{
    binding: RouteBinding<S, Request>,
    warnings: Vec<Cow<'static, str>>,
    request: PhantomData<Request>,
}

impl<S, Request> fmt::Debug for RouteBinding<S, Request>
where
    S: Service<Request>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteBinding")
            .field("id", &self.id)
            .field("metadata", &self.metadata)
            .field("effective_qos", &self.effective_qos)
            .field("effective_security", &self.effective_security)
            .finish()
    }
}

impl<S, Request> fmt::Debug for RouteDecision<S, Request>
where
    S: Service<Request>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteDecision")
            .field("binding", &self.binding)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl<S, Request> RouteDecision<S, Request>
where
    S: Service<Request>,
{
    /// 构造决策结果。
    pub fn new(binding: RouteBinding<S, Request>, warnings: Vec<Cow<'static, str>>) -> Self {
        Self {
            binding,
            warnings,
            request: PhantomData,
        }
    }

    /// 访问绑定。
    pub fn binding(&self) -> &RouteBinding<S, Request> {
        &self.binding
    }

    /// 访问绑定（可变）。
    pub fn binding_mut(&mut self) -> &mut RouteBinding<S, Request> {
        &mut self.binding
    }

    /// 访问告警列表。
    pub fn warnings(&self) -> &[Cow<'static, str>] {
        &self.warnings
    }
}

/// `RouteValidation` 提供在控制面写入路由前的预检结果。
///
/// # 设计动机（Why）
/// - 结合 Envoy xDS/Lua 校验、Istio 分析工具的经验，在契约层提供统一接口，
///   以便不同实现共享一致的校验报告格式。
///
/// # 字段说明（What）
/// - `errors`：阻断下发的错误集合。
/// - `warnings`：可观测但不阻断的提示。
#[derive(Debug, Default)]
pub struct RouteValidation {
    errors: Vec<Cow<'static, str>>,
    warnings: Vec<Cow<'static, str>>,
}

impl RouteValidation {
    /// 创建空白校验结果。
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加错误。
    pub fn push_error(&mut self, message: Cow<'static, str>) {
        self.errors.push(message);
    }

    /// 添加告警。
    pub fn push_warning(&mut self, message: Cow<'static, str>) {
        self.warnings.push(message);
    }

    /// 返回错误列表。
    pub fn errors(&self) -> &[Cow<'static, str>] {
        &self.errors
    }

    /// 返回告警列表。
    pub fn warnings(&self) -> &[Cow<'static, str>] {
        &self.warnings
    }

    /// 判断是否通过校验。
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}
