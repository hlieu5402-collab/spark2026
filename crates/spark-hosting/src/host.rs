use spark_core::configuration::{BuildReport, ConfigurationHandle, ResolvedConfiguration};

use crate::{MiddlewareRegistry, ServiceRegistry};

/// `Host` 封装宿主运行时所需的核心构件。
///
/// # 教案级注释
/// - **设计目的 (Why)**
///   - 将配置句柄、服务注册表与中间件目录集中保存在一个结构体中，
///     便于宿主在完成装配后以单一入口向下传递依赖。
///   - 提供对初始配置与构建报告的访问，方便审计与可观测组件在启动时记录状态。
/// - **体系位置 (Where)**
///   - 该结构由 [`HostBuilder`](crate::builder::HostBuilder) 生成，
///     并在运行时启动阶段交由宿主主循环持有。
/// - **关键要素 (How)**
///   - `configuration_handle`：用于后续读取/订阅配置；
///   - `initial_configuration`：首个解析结果，便于快速渲染诊断信息；
///   - `configuration_report`：校验 & 装配报告，帮助定位构建失败原因；
///   - `services` 与 `middleware`：提供对数据平面组件的有序访问。
/// - **契约说明 (What)**
///   - 结构体自身不执行任何 I/O，仅存放装配阶段的产物；
///   - 调用方在读取配置句柄的可变引用时，需确保不会在多线程环境下产生数据竞争。
/// - **风险提示 (Trade-offs)**
///   - 当前未内置生命周期管理或热更新逻辑；若宿主需要在运行时动态调整注册表，
///     请自行在外部加锁或复制必要的数据结构。
pub struct Host {
    configuration_handle: ConfigurationHandle,
    initial_configuration: ResolvedConfiguration,
    configuration_report: BuildReport,
    services: ServiceRegistry,
    middleware: MiddlewareRegistry,
}

impl core::fmt::Debug for Host {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let profile = self.configuration_handle.profile();
        let service_count = self.services.iter().count();
        let middleware_count = self.middleware.iter().count();
        f.debug_struct("Host")
            .field("profile", &profile.identifier.as_str())
            .field("service_count", &service_count)
            .field("middleware_count", &middleware_count)
            .finish()
    }
}

impl Host {
    /// 以构建成果创建宿主实例。
    pub(crate) fn new(
        configuration_handle: ConfigurationHandle,
        initial_configuration: ResolvedConfiguration,
        configuration_report: BuildReport,
        services: ServiceRegistry,
        middleware: MiddlewareRegistry,
    ) -> Self {
        Self {
            configuration_handle,
            initial_configuration,
            configuration_report,
            services,
            middleware,
        }
    }

    /// 获取配置句柄的可变引用，以便读取快照或订阅增量更新。
    ///
    /// - **前置条件**：调用方在多线程环境中必须保证互斥访问；
    /// - **后置条件**：返回的引用在使用期间保持有效，宿主仍拥有所有权。
    pub fn configuration_handle(&mut self) -> &mut ConfigurationHandle {
        &mut self.configuration_handle
    }

    /// 返回初始化阶段解析出的配置快照。
    ///
    /// - **语义说明**：该快照在 Host 构建完成时即固定，不随后续增量更新而变化；
    /// - **使用建议**：可用于启动日志或健康检查的静态基线。
    pub fn initial_configuration(&self) -> &ResolvedConfiguration {
        &self.initial_configuration
    }

    /// 获取配置构建报告，包含校验记录与脱敏快照摘要。
    pub fn configuration_report(&self) -> &BuildReport {
        &self.configuration_report
    }

    /// 访问服务注册表的只读视图。
    ///
    /// - **风险提示**：返回值可被克隆并独立使用，但不会自动跟踪运行时状态变化。
    pub fn services(&self) -> &ServiceRegistry {
        &self.services
    }

    /// 访问中间件注册表。
    pub fn middleware(&self) -> &MiddlewareRegistry {
        &self.middleware
    }
}
