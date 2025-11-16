use alloc::sync::Arc;
use core::future;
use core::task::{Context as TaskContext, Poll};

#[cfg(feature = "router-factory")]
use spark_core::service::{BoxService, auto_dyn::bridge_to_box_service};
use spark_core::{
    CallContext, Context, PipelineMessage, SparkError,
    service::Service,
    status::{PollReady, ReadyCheck, ReadyState},
};
#[cfg(feature = "router-factory")]
use spark_router::ServiceFactory;

use crate::core::SessionManager;

use super::location::LocationStore;

/// `ProxyService` 尚未接入 INVITE 编排时返回的占位错误码。
const CODE_PROXY_UNIMPLEMENTED: &str = "switch.proxy.unimplemented";

/// `ProxyService` 承载 B2BUA 的 INVITE 处理入口。
///
/// # 教案式解读
/// - **意图 (Why)**：
///   - 聚合 [`LocationStore`]（终端位置存储）与 [`SessionManager`]（会话仓储），
///     为后续 A/B leg 编排提供统一的对象层 Service 实例；
///   - 作为 SIP Proxy/B2BUA 的入口 Handler，后续会解析 INVITE 并驱动双腿。
/// - **架构位置 (Where)**：
///   - 隶属于 `spark-switch::applications`，通常由 `spark_router` 根据路由
///     规则按需构造；
///   - 运行于对象层 Service 框架，与 [`spark_core::service::Service`] 契约对齐。
/// - **契约 (What)**：
///   - 必须持有 `LocationStore` 与 `SessionManager` 的共享引用以便在多实例
///     环境复用注册表与会话状态；
///   - `call` 返回 [`PipelineMessage`]，以保持与其他应用 Service 的一致性。
/// - **风险提示 (Trade-offs)**：当前实现仍返回占位错误，后续迭代需补齐
///   INVITE 解析、会话创建等逻辑；占位错误码 `switch.proxy.unimplemented`
///   能帮助上层快速识别尚未完成功能。
#[derive(Debug)]
pub struct ProxyService {
    location_store: Arc<LocationStore>,
    session_manager: Arc<SessionManager>,
}

impl ProxyService {
    /// 以共享状态构造 ProxyService。
    ///
    /// # 契约说明
    /// - **输入参数**：
    ///   - `location_store`：Registrar 注册表的 `Arc`，调用方需确保生命周期
    ///     长于 Service；
    ///   - `session_manager`：会话仓储的 `Arc`，负责跨线程维护 B2BUA 状态；
    /// - **前置条件**：调用方必须保证传入的 `Arc` 已经完成初始化，且能够在
    ///   Service 生命周期内保持可用；
    /// - **后置条件**：返回的实例实现 `Service<PipelineMessage>`，可直接交由
    ///   路由器调度。
    #[must_use]
    pub fn new(location_store: Arc<LocationStore>, session_manager: Arc<SessionManager>) -> Self {
        Self {
            location_store,
            session_manager,
        }
    }

    /// 处理输入的 INVITE 报文。
    ///
    /// # 教案式注释
    /// - **意图 (Why)**：集中封装 INVITE → B2BUA 状态机的处理入口，方便后续在
    ///   内部拆分解析、会话创建、B-leg 调度等子步骤；
    /// - **解析逻辑 (How)**：当前仅占位，直接返回 `switch.proxy.unimplemented`
    ///   错误；后续版本会根据 `PipelineMessage` 的 SIP 内容执行分支；
    /// - **契约 (What)**：
    ///   - **输入**：完整的 [`PipelineMessage`]，期望携带 SIP INVITE 字节；
    ///   - **输出**：`Result<PipelineMessage, SparkError>`，成功时返回下游应发
    ///     响应，失败时返回领域错误；
    ///   - **前置条件**：调用方应确保 `location_store`、`session_manager` 在构造
    ///     时已注入；
    ///   - **后置条件**：当前版本必定返回错误，提示功能尚未实现；
    /// - **风险提示 (Trade-offs)**：保留 `Err` 路径可避免外部误以为功能已可用，
    ///   同时使测试能够断言未来行为；TODO 逻辑会在后续 Epic 中补全。
    #[allow(clippy::result_large_err)]
    fn handle_request(
        &self,
        _message: PipelineMessage,
    ) -> spark_core::Result<PipelineMessage, SparkError> {
        let _ = &self.location_store;
        let _ = &self.session_manager;

        Err(SparkError::new(
            CODE_PROXY_UNIMPLEMENTED,
            "ProxyService::handle_request 尚未实现 INVITE 调度路径",
        ))
    }
}

impl Service<PipelineMessage> for ProxyService {
    type Response = PipelineMessage;
    type Error = SparkError;
    type Future = future::Ready<spark_core::Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &Context<'_>, _: &mut TaskContext<'_>) -> PollReady<Self::Error> {
        Poll::Ready(ReadyCheck::Ready(ReadyState::Ready))
    }

    fn call(&mut self, _ctx: CallContext, req: PipelineMessage) -> Self::Future {
        future::ready(self.handle_request(req))
    }
}

/// `ProxyServiceFactory` 将共享状态打包成可供路由器按需克隆的工厂。
///
/// # 教案式说明
/// - **意图 (Why)**：`spark_router` 在命中路由时会调用 `ServiceFactory::create`
///   生成新的对象层 Service，本结构封装 `Arc` 注入逻辑，避免每条路由重复编写
///   构造器；
/// - **契约 (What)**：实现 [`ServiceFactory`]，输出 `BoxService`；
/// - **架构 (Where)**：与 `ProxyService` 位于同一模块，由宿主在路由表更新时注入。
#[cfg(feature = "router-factory")]
#[derive(Clone, Debug)]
pub struct ProxyServiceFactory {
    location_store: Arc<LocationStore>,
    session_manager: Arc<SessionManager>,
}

#[cfg(feature = "router-factory")]
impl ProxyServiceFactory {
    /// 构造注入共享状态的工厂实例。
    ///
    /// - **输入**：与 [`ProxyService::new`] 相同的共享引用；
    /// - **前置条件**：传入的 `Arc` 需在多线程环境可安全复用；
    /// - **后置条件**：可交由 `spark_router` 存储并在命中路由后构造 Service。
    #[must_use]
    pub fn new(location_store: Arc<LocationStore>, session_manager: Arc<SessionManager>) -> Self {
        Self {
            location_store,
            session_manager,
        }
    }
}

#[cfg(feature = "router-factory")]
impl ServiceFactory for ProxyServiceFactory {
    fn create(&self) -> spark_core::Result<BoxService, SparkError> {
        let service = ProxyService::new(
            Arc::clone(&self.location_store),
            Arc::clone(&self.session_manager),
        );
        Ok(bridge_to_box_service(service))
    }
}
