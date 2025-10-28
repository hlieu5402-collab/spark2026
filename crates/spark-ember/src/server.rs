use alloc::{boxed::Box, sync::Arc};

use spark_core::{
    Result as CoreResult,
    contract::CallContext,
    pipeline::DynControllerFactory,
    runtime::CoreServices,
    transport::{DynServerTransport, DynTransportFactory, ListenerConfig},
};
use spin::Mutex;

use crate::transport::TransportFactoryExt;

/// 将 `DynControllerFactory` 再包装为对象安全别名，方便在宿主 API 中使用。
pub trait ControllerFactory: DynControllerFactory {}

impl<T> ControllerFactory for T where T: DynControllerFactory {}

/// `EmberServer` 负责在运行时协调 Transport 监听器与 Pipeline 控制面的装配。
///
/// # 教案级说明
///
/// ## 意图 (Why)
/// - 将 `CoreServices`、传输工厂与监听配置集中管理，使接入层能够在不感知具体协议实现的前提下启动服务；
/// - 为后续的连接接受、优雅关闭与观测打下地基：一旦有新的协议实现（TCP/QUIC 等）注册到工厂中，即可复用同一宿主逻辑。
///
/// ## 解析逻辑 (How)
/// - 构造阶段记录运行时依赖（`CoreServices`）、监听配置与对象层 `DynTransportFactory`；
/// - `run` 方法根据默认 `CallContext` 派生 `ExecutionContext`，调用扩展的 `listen` 启动监听器，并将返回的 `DynServerTransport` 缓存；
/// - 监听器缓存使用 `spin::Mutex` 封装，保证在 `no_std + alloc` 环境中同样安全；后续的优雅关闭可以复用该缓存。
///
/// ## 契约定义 (What)
/// - 构造函数参数：
///   - `services`：`CoreServices` 聚合的运行时能力；
///   - `config`：`ListenerConfig`，描述端点、并发上限等监听策略；
///   - `transport_factory`：对象层传输工厂，负责实际建连与握手。
/// - 运行时行为：`run` 返回 `CoreResult<()>`，失败时向上传递结构化 `CoreError`；监听器引用在成功路径中被缓存以供后续管理。
///
/// ## 风险与权衡 (Trade-offs & Gotchas)
/// - 当前 `run` 仅负责启动监听并缓存句柄，真正的连接接受将在后续迭代补充；
/// - 为兼容 `no_std`，选择 `spin::Mutex` 而非 `std::sync::Mutex`；在高并发场景需注意自旋锁对 CPU 的影响，但监听器启动属于冷路径，可接受该开销；
/// - 缓存监听器意味着 `EmberServer` 必须在停机路径显式释放资源，后续实现需确保 `shutdown` 时清理该字段。
pub struct EmberServer {
    services: CoreServices,
    config: ListenerConfig,
    transport_factory: Arc<dyn DynTransportFactory>,
    listener: Mutex<Option<Box<dyn DynServerTransport>>>,
}

impl EmberServer {
    /// 构造 `EmberServer`，聚合运行时服务与传输工厂。
    ///
    /// # 参数说明
    /// - `services`: 核心运行时能力集合，供后续接受任务、指标上报等场景使用；
    /// - `config`: 监听配置，需与传输工厂声明的协议一致；
    /// - `transport_factory`: 对象层传输工厂，实现具体协议的监听/握手逻辑。
    ///
    /// # 契约定义
    /// - **前置条件**：传入的 `CoreServices` 与 `DynTransportFactory` 必须已经初始化完毕，且在服务器生命周期内保持有效；
    /// - **后置条件**：返回的结构体仅完成依赖记录，不会立即启动监听，也不会复制底层资源（`CoreServices` 内部均通过 `Arc` 分享）。
    pub fn new(
        services: CoreServices,
        config: ListenerConfig,
        transport_factory: Arc<dyn DynTransportFactory>,
    ) -> Self {
        Self {
            services,
            config,
            transport_factory,
            listener: Mutex::new(None),
        }
    }

    /// 启动监听器并缓存返回的传输句柄。
    ///
    /// # 教案式注释
    ///
    /// ## 意图 (Why)
    /// - 在统一入口中完成监听器初始化，使控制平面或测试桩只需提供 Pipeline 工厂即可启动服务；
    /// - 为后续的连接接受循环做准备：缓存监听器句柄，便于添加健康检查与优雅关闭逻辑。
    ///
    /// ## 解析逻辑 (How)
    /// 1. 构造最小化的 [`CallContext`] 并派生 `ExecutionContext`，确保 `listen` 调用遵循取消/截止契约；
    /// 2. 调用扩展方法 `TransportFactoryExt::listen`，将监听配置与 Pipeline 工厂交给传输实现；
    /// 3. 使用内部 `Mutex` 缓存成功返回的 `DynServerTransport`，自动释放旧句柄；
    /// 4. 暂未开启实际的接受循环——`DynServerTransport` 目前尚未公开 `accept` 契约，待后续任务补齐。
    ///
    /// ## 契约定义 (What)
    /// - `controller_factory`: 负责构建 Pipeline 的对象层工厂，调用方需确保其中 Handler 满足线程安全；
    /// - 返回：`CoreResult<()>`，成功表示监听器已就绪并被缓存，失败时传播底层 `CoreError`。
    /// - **前置条件**：`EmberServer` 已通过 [`Self::new`] 正确初始化；
    /// - **后置条件**：`self.listener` 保存最新监听器，供后续关闭或观测；尚未开始消费连接。
    ///
    /// ## 风险提示 (Trade-offs & Gotchas)
    /// - 当前缺乏实际的接受循环，因此调用者仍需结合外部任务或后续版本完成连接处理；
    /// - 若 `listen` 阶段失败，缓存保持原状，不会覆盖旧的监听器；
    /// - TODO(ember-accept-loop)：待 `DynServerTransport` 暴露 `accept` 契约后，需要在此方法中启动异步循环创建 Pipeline，以避免长时间只监听不处理。
    pub async fn run(&self, controller_factory: Arc<dyn ControllerFactory>) -> CoreResult<()> {
        let call_context = CallContext::builder().build();
        let execution = call_context.execution();

        let listener = self
            .transport_factory
            .listen(&execution, self.config.clone(), controller_factory)
            .await?;

        let mut slot = self.listener.lock();
        *slot = Some(listener);

        Ok(())
    }

    /// 访问运行时服务，便于测试或上层在运行期间查询注入的能力。
    pub fn services(&self) -> &CoreServices {
        &self.services
    }
}
