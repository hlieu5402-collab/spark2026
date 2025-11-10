use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};

use spark_core::{
    Result as CoreResult,
    async_trait,
    buffer::PipelineMessage,
    contract::{CallContext, CloseReason},
    error::CoreError,
    pipeline::{
        ChainBuilder, Context as PipelineContext, InitializerDescriptor, InboundHandler,
        OutboundHandler, PipelineInitializer, WriteSignal,
    },
    runtime::CoreServices,
    service::BoxService,
    transport::{
        Capability, CapabilityBitmap, DowngradeReport, DynServerChannel, DynTransportFactory,
        HandshakeOutcome, ListenerConfig, Version,
    },
};
use spark_hosting::{Host, MiddlewareRegistry, ServiceEntry, ServiceRegistry};
use spin::Mutex;

/// `PipelineFactory`：为 `DynPipelineFactory` 提供更直观的对象安全别名。
///
/// # Why
/// - 教学中希望隐藏 `DynPipelineFactory` 的复杂类型名称；
/// - 强调“任何实现了 `DynPipelineFactory` 的类型都可以被放入 `Arc<dyn PipelineFactory>`”。
///
/// # How
/// - 通过空 trait 继承语法，为所有实现 `DynPipelineFactory` 的类型自动实现本 trait。
///
/// # 合同
/// - *前置条件*：类型已经实现 `DynPipelineFactory`；
/// - *后置条件*：可以安全地将对象封装为 `Arc<dyn PipelineFactory>` 并在示例中传递。
pub trait PipelineFactory: spark_core::pipeline::DynPipelineFactory {}

impl<T> PipelineFactory for T where T: spark_core::pipeline::DynPipelineFactory {}

/// `ExamplesServer` 演示如何串联 Host、Transport 与 Pipeline 装配。
///
/// # 教案级总览
///
/// ## 意图 (Why)
/// - 在新一代启动链路中，宿主需要同时处理“配置/服务注册/中间件装配”与“传输监听器绑定”；
/// - 本结构体将 Host 暴露的中间件与服务注册表接入传输监听器的协议协商流程，演示如何以声明式方式完成 L1 → L2 → Service 的逐层路由；
/// - 对比旧版样例，新增的协议协商闭包将 L1 路由逻辑回收到 `ServerChannel` 内部，使多协议监听器能够动态挑选 `PipelineInitializer`。
///
/// ## 构成 (What)
/// - `services`：`CoreServices` 聚合体，向 Handler 暴露运行时依赖；
/// - `config`：`ListenerConfig`，传输层监听所需参数；
/// - `transport_factory`：对象层传输工厂；
/// - `host`：由 `spark-hosting` 构建的宿主，用于查找中间件与服务；
/// - `full_stack_label` / `minimal_stack_label`：在 Host 注册表中查找具体初始化器的键；
/// - `listener`：缓存启动后的 `DynServerChannel`，便于后续优雅关闭或观测；
/// - `service_name`：业务服务在注册表中的名称，供初始化器将 Service 适配器纳入 Handler 链。
///
/// ## 协同流程 (How)
/// 1. `run` 方法先调用传输工厂 `listen` 启动监听；
/// 2. 随后从 Host 的中间件目录、服务注册表中提取 `Arc<dyn PipelineInitializer>` 与 `BoxService`；
/// 3. 通过 `ServerChannel::set_initializer_selector` 注入协议协商闭包：
///    - 若握手能力包含多路复用，则选择“全链路”初始化器（含 L2 AppRouter）；
///    - 否则回退到“最小链路”初始化器，仅保留编解码与 Service 适配；
/// 4. 初始化器会在 `configure` 阶段按顺序安装 `CodecHandler` → 可选的 `AppRouterHandler` → `ServiceAdapterHandler`；
/// 5. 成功后缓存监听器句柄，运行时即可在外部触发 `accept` 循环。
///
/// ## 契约约束
/// - **前置条件**：
///   - Host 需提前注册 `service_name` 对应的 `BoxService`，以及两个具名的 `PipelineInitializer`；
///   - `transport_factory` 与 `config` 的协议需匹配；
/// - **后置条件**：
///   - `run` 成功返回后，监听器已经安装好协议协商闭包并处于就绪状态；
///   - `listener` 缓存中保存最新的 `DynServerChannel`。
///
/// ## 风险提示 (Trade-offs & Gotchas)
/// - 示例中的 Service 适配器仅做结构化演示，没有真正驱动 `BoxService` 调用；实际项目应在 Handler 中通过执行器调度业务 Future；
/// - Host 的配置源加载逻辑依赖宿主侧实现，本示例假定 Host 已由外部装配完成；
/// - 若握手能力声明与 Host 中注册的初始化器不符，示例会 panic 以凸显配置不一致，生产环境需改为返回结构化错误。
pub struct ExamplesServer {
    services: CoreServices,
    config: ListenerConfig,
    transport_factory: Arc<dyn DynTransportFactory>,
    host: Host,
    full_stack_label: String,
    minimal_stack_label: String,
    service_name: String,
    listener: Mutex<Option<Box<dyn DynServerChannel>>>,
}

impl ExamplesServer {
    /// 构造 `ExamplesServer`，集中注入所有启动所需资源。
    ///
    /// # Why
    /// - 强调“构造阶段完成配置绑定，运行阶段尽量只读”；
    /// - 便于测试时通过依赖注入替换 Host/Transport 等组件。
    ///
    /// # How
    /// - 保存运行时 `CoreServices`、监听配置、传输工厂与 Host 引用；
    /// - 将链路标签与服务名称转换成 `String`，以便日志输出与注册表查询；
    /// - 初始化监听器缓存为 `None`，待 `run` 后填入。
    ///
    /// # 合同
    /// - *参数*：
    ///   - `services`：运行时能力聚合体；
    ///   - `config`：监听配置；
    ///   - `transport_factory`：对象层传输工厂；
    ///   - `host`：已完成注册的宿主；
    ///   - `full_stack_label` / `minimal_stack_label`：Host 中查询初始化器的键；
    ///   - `service_name`：Service 注册表中目标业务服务的名称；
    /// - *前置条件*：Host 中确实存在所需初始化器或允许示例构造默认实现；
    /// - *后置条件*：返回的服务器实例可直接进入 `run` 阶段。
    ///
    /// # 风险
    /// - 为突出教学示例，构造函数未对标签有效性进行提前校验。
    pub fn new(
        services: CoreServices,
        config: ListenerConfig,
        transport_factory: Arc<dyn DynTransportFactory>,
        host: Host,
        full_stack_label: impl Into<String>,
        minimal_stack_label: impl Into<String>,
        service_name: impl Into<String>,
    ) -> Self {
        Self {
            services,
            config,
            transport_factory,
            host,
            full_stack_label: full_stack_label.into(),
            minimal_stack_label: minimal_stack_label.into(),
            service_name: service_name.into(),
            listener: Mutex::new(None),
        }
    }

    /// 启动监听器并安装协议协商逻辑。
    ///
    /// # Why
    /// - 展示“Transport 监听 + Host 协商”的一站式调用路径；
    /// - 教导读者在握手后如何根据能力集切换 Handler 链路。
    ///
    /// # How
    /// - 基于 `CallContext` 获取执行环境并调用 `transport_factory.listen`；
    /// - 构造 `InitializerSelectorContext` 收集所需初始化器；
    /// - 生成选择器闭包并交给 `ServerChannel`；
    /// - 缓存监听器句柄，便于后续关闭或观测。
    ///
    /// # 合同
    /// - *参数*：`pipeline_factory` —— Transport 启动所需的 Pipeline 工厂；
    /// - *返回*：`CoreResult<()>`，成功即表示监听器就绪；
    /// - *前置条件*：传输工厂与监听配置匹配，Host 提供必要的初始化器与服务；
    /// - *后置条件*：监听器缓存被填充，协议选择器已经设置。
    ///
    /// # 风险
    /// - 若 Host 缺失依赖会 panic；
    /// - 函数未驱动 `accept` 循环，调用方需额外调度连接处理逻辑。
    pub async fn run(&self, pipeline_factory: Arc<dyn PipelineFactory>) -> CoreResult<()> {
        let call_context = CallContext::builder().build();
        let execution = call_context.execution();

        let listener = self
            .transport_factory
            .listen(&execution, self.config.clone(), pipeline_factory)
            .await?;

        let selectors = InitializerSelectorContext::new(
            &self.host,
            &self.full_stack_label,
            &self.minimal_stack_label,
            &self.service_name,
        );

        let selector = selectors.into_selector();
        listener.set_initializer_selector_dyn(selector);

        let mut slot = self.listener.lock();
        *slot = Some(listener);
        Ok(())
    }

    /// 提供对运行时服务的访问，方便测试与演示代码查询注入的能力。
    ///
    /// # Why
    /// - 单元测试可通过该接口验证运行时依赖是否注入；
    /// - 读者可探索 `CoreServices` 包含的工具以进一步扩展示例。
    ///
    /// # How
    /// - 简单返回内部 `CoreServices` 的引用。
    ///
    /// # 合同
    /// - *返回*：共享引用 `&CoreServices`；
    /// - *前置条件*：无；
    /// - *后置条件*：保持不可变借用，调用方不能修改内部状态。
    pub fn services(&self) -> &CoreServices {
        &self.services
    }
}

/// 协商上下文：从 Host 抽取初始化器与服务引用，并构建协议选择闭包。
/// `InitializerSelectorContext` 负责从宿主查询初始化器与服务实例，并将其编排成协议选择闭包。
///
/// # 教案级解读
/// - **意图**：将 Host 注册表中的“全链路 / 精简链路”初始化器包装成 `ServerChannel` 可识别的闭包，统一处理握手能力判定与初始化器分发。
/// - **定位**：位于 Listener 装配流程的 L1 路由阶段，直接服务于 `ExamplesServer::run`；其产物会被 `ServerChannel::set_initializer_selector_dyn` 调用。
/// - **关键逻辑**：
///   1. 通过 `resolve_service_instance` 取得业务 Service，确保两个初始化器共享同一 Service 引用；
///   2. 分别调用 `resolve_initializer`，若 Host 未预注册则即时构建 `MyInitializer`；
///   3. 构建闭包，基于握手能力位图挑选适合的初始化器。
/// - **契约**：
///   - *输入*：Host 引用、两个初始化器标签、服务名称；
///   - *前置条件*：Host 必须至少提供 `service_name` 对应的 Service 或工厂；
///   - *后置条件*：`into_selector` 返回的闭包可安全克隆、在多线程场景中复用。
/// - **考量**：
///   - 若 Host 未注册给定标签的初始化器，本示例选择即时构造 `MyInitializer`，牺牲了一部分显式配置换取教学清晰度；
///   - 握手能力采用能力位图匹配，允许未来扩展更多能力位判断逻辑。
struct InitializerSelectorContext {
    full_stack: Arc<dyn PipelineInitializer>,
    minimal_stack: Arc<dyn PipelineInitializer>,
}

impl InitializerSelectorContext {
    /// 从 Host 抽取运行所需的初始化器与服务引用。
    ///
    /// # Why
    /// - 在启动阶段一次性查齐所有依赖，避免闭包执行时再访问 Host 造成竞争；
    /// - 确保“全链路 / 精简链路”使用相同的 Service 实例，减少状态分裂。
    ///
    /// # How
    /// - 读取宿主的中间件/服务注册表；
    /// - 先解析 Service，再为两个标签分别解析/构造 `MyInitializer`；
    /// - 返回携带引用的上下文对象。
    ///
    /// # 合同
    /// - *参数*：`host` 为宿主实例；`full_label` / `minimal_label` 为中间件注册表 key；`service_name` 为服务注册表 key；
    /// - *返回值*：包含两个 `Arc<dyn PipelineInitializer>` 的上下文。
    /// - *前置条件*：Host 中的服务注册表必须能解析 `service_name`；
    /// - *后置条件*：上下文内的引用均已准备就绪，可立即进入闭包构建阶段。
    ///
    /// # 风险提示
    /// - 若服务或初始化器缺失，函数会 panic；示例通过极端反馈提醒读者校验配置。
    fn new(
        host: &Host,
        full_label: &str,
        minimal_label: &str,
        service_name: &str,
    ) -> Self {
        let middleware = host.middleware();
        let services = host.services();

        let service = resolve_service_instance(services, service_name);

        let full_stack = resolve_initializer(middleware, full_label, service.clone(), true);
        let minimal_stack = resolve_initializer(middleware, minimal_label, service, false);

        Self {
            full_stack,
            minimal_stack,
        }
    }

    /// 将上下文转换为 `ServerChannel` 所需的协议选择闭包。
    ///
    /// # Why
    /// - ServerChannel 在握手完成后需要基于能力集决定具体初始化器；
    /// - 将判断逻辑集中在此，便于测试与复用。
    ///
    /// # How
    /// - 构造 `CapabilityBitmap`，要求含有 `MULTIPLEXING` 能力时走全链路；
    /// - 否则回退到精简链路；
    /// - 返回 `Arc` 包裹的闭包，满足对象安全要求。
    ///
    /// # 合同
    /// - *输入*：消费当前上下文；
    /// - *输出*：可被多线程共享的闭包；
    /// - *前置条件*：上下文内初始化器非空；
    /// - *后置条件*：闭包在每次握手后返回有效的 `PipelineInitializer`。
    ///
    /// # 考量
    /// - 能力判定使用“子集”语义，可兼容将来叠加的能力；
    /// - 通过 `Arc::clone` 避免闭包捕获所有权，兼容多连接并发。
    fn into_selector(self) -> Arc<spark_core::transport::PipelineInitializerSelector> {
        let Self {
            full_stack,
            minimal_stack,
        } = self;

        let mut requires_router = CapabilityBitmap::empty();
        requires_router.insert(Capability::MULTIPLEXING);

        Arc::new(move |outcome: &HandshakeOutcome| {
            if requires_router.is_subset_of(outcome.capabilities()) {
                Arc::clone(&full_stack)
            } else {
                Arc::clone(&minimal_stack)
            }
        })
    }
}

/// 根据标签解析或构造 `PipelineInitializer`。
///
/// # Why
/// - 中间件注册表允许事先注册，也允许示例在缺失时即时提供默认实现；
/// - 统一入口可复用“构造描述符 + 绑定 Service”逻辑。
///
/// # How
/// - 尝试在注册表中查询目标标签；
/// - 若存在则直接克隆 `Arc`；
/// - 否则创建 `MyInitializer`，其中 `enable_router` 控制 Handler 组合。
///
/// # 合同
/// - *参数*：`registry` 中间件注册表；`label` 查找键；`service` 已解析的业务 Service；`enable_router` 决定是否挂载 L2 路由；
/// - *返回*：满足对象安全要求的 `Arc<dyn PipelineInitializer>`；
/// - *前置条件*：`service` 必须有效，且调用方负责在多处复用同一实例；
/// - *后置条件*：返回值总是非空，便于调用方直接使用。
///
/// # 风险
/// - 若初始化器缺失即构造默认实现，可能掩盖配置缺陷；示例中选择此策略以方便演示。
fn resolve_initializer(
    registry: &MiddlewareRegistry,
    label: &str,
    service: BoxService,
    enable_router: bool,
) -> Arc<dyn PipelineInitializer> {
    let descriptor = format!(
        "spark.examples.pipeline.{}",
        if enable_router { "full" } else { "minimal" }
    );

    if let Some(initializer) = registry.get(label) {
        Arc::clone(initializer)
    } else {
        Arc::new(MyInitializer::new(
            descriptor,
            enable_router,
            service,
        ))
    }
}

/// 从宿主的服务注册表解析业务 Service 实例。
///
/// # Why
/// - Handler 链的 Service 适配器需要具体实例才能运行；
/// - 注册表既支持直接实例，也支持工厂函数，本函数统一两者。
///
/// # How
/// - 命中实例分支时直接克隆；
/// - 命中工厂分支时即时创建；
/// - 若缺失则 panic，强调配置是示例成功的前提。
///
/// # 合同
/// - *参数*：`registry` 服务注册表，`name` 服务标识；
/// - *返回*：`BoxService` 动态对象；
/// - *前置条件*：注册表中存在对应项；
/// - *后置条件*：返回的 Service 可被多路复用。
///
/// # 风险
/// - 工厂调用可能失败，示例中直接 unwrap 以简化流程，真实环境需处理错误。
fn resolve_service_instance(registry: &ServiceRegistry, name: &str) -> BoxService {
    match registry.get(name) {
        Some(ServiceEntry::Instance(service)) => service.clone(),
        Some(ServiceEntry::Factory(factory)) => factory
            .create()
            .expect("service factory returned error in example"),
        None => panic!("missing service `{name}` in host service registry"),
    }
}

/// 示例初始化器：组合编解码、可选应用路由与 Service 适配器。
struct MyInitializer {
    descriptor: InitializerDescriptor,
    enable_router: bool,
    service: BoxService,
}

impl MyInitializer {
    /// 构造 `MyInitializer`，绑定链路描述与业务 Service。
    ///
    /// # Why
    /// - 将标签、类别、描述集中在此构造，便于一次性配置链路元信息；
    /// - `enable_router` 控制是否挂载 L2 Handler，支撑协议能力差异化。
    ///
    /// # How
    /// - 生成描述符：name 采用 label，category 依据 `enable_router` 区分；
    /// - 保存 Service 引用以供 `configure` 时安装 Handler。
    ///
    /// # 合同
    /// - *参数*：`label` 用于标识链路；`enable_router` 指示 Handler 组合；`service` 业务 Service；
    /// - *返回*：初始化器实例，后续通过 `PipelineInitializer` 接口使用；
    /// - *前置条件*：`service` 必须有效；
    /// - *后置条件*：结构体内缓存 `descriptor` / `service`，等待 `configure` 调用。
    ///
    /// # 风险
    /// - 描述符文本写死在示例中，真实环境需国际化/配置化处理。
    fn new(label: impl Into<String>, enable_router: bool, service: BoxService) -> Self {
        let name = format!("{}", label.into());
        let descriptor = InitializerDescriptor::new(
            name.into(),
            if enable_router { "routing" } else { "codec" }.into(),
            if enable_router {
                "全功能链路：编解码 + 应用路由 + Service 适配"
            } else {
                "精简链路：仅包含编解码与 Service 适配"
            }
            .into(),
        );

        Self {
            descriptor,
            enable_router,
            service,
        }
    }
}

impl PipelineInitializer for MyInitializer {
    fn descriptor(&self) -> InitializerDescriptor {
        self.descriptor.clone()
    }

    /// 安装 Handler 链，串联编解码、可选路由与 Service 适配。
    ///
    /// # Why
    /// - `PipelineInitializer` 的职责是针对特定协议组合 Handler；
    /// - 示例希望显式展示 Handler 顺序以及各自职责。
    ///
    /// # How
    /// - 注册编解码 Handler（进/出双向）；
    /// - 在 `enable_router` 为真时插入 L2 路由 Handler；
    /// - 最后安装 Service 适配器，将请求交给业务 Service。
    ///
    /// # 合同
    /// - *参数*：`chain` 为 Handler 注册入口；`_services` 提供全局运行时（本示例未用）；
    /// - *返回*：成功与否的 `CoreResult`；
    /// - *前置条件*：`chain` 尚未注册同名 Handler；
    /// - *后置条件*：链路按照 Codec → (Router) → Service Adapter 的顺序装配完成。
    ///
    /// # 风险
    /// - Handler 名称写死在示例中，真实项目需考虑命名冲突；
    /// - 未安装出站 Service Handler，仅为保持示例精简。
    fn configure(
        &self,
        chain: &mut dyn ChainBuilder,
        _services: &CoreServices,
    ) -> CoreResult<(), CoreError> {
        let codec_descriptor = InitializerDescriptor::new(
            "spark.examples.codec",
            "codec",
            "负责协议层编解码并桥接 Pipeline 消息格式",
        );
        let codec_handler = CodecHandler::new(codec_descriptor.clone());
        chain.register_inbound("codec.inbound", Box::new(codec_handler.clone()));
        chain.register_outbound("codec.outbound", Box::new(codec_handler));

        if self.enable_router {
            let router_descriptor = InitializerDescriptor::new(
                "spark.examples.router",
                "routing",
                "根据帧头元数据选择业务路由",
            );
            chain.register_inbound("router.l2", Box::new(AppRouterHandler::new(router_descriptor)));
        }

        let service_descriptor = InitializerDescriptor::new(
            "spark.examples.service_adapter",
            "service",
            "将 Pipeline 消息交给业务 Service，并在完成后继续写出响应",
        );
        chain.register_inbound(
            "service.adapter",
            Box::new(ServiceAdapterHandler::new(service_descriptor, self.service.clone())),
        );

        Ok(())
    }
}

/// `CodecHandler`：演示如何同时实现入站与出站 Handler。
///
/// # Why
/// - Pipeline 初始化器常见的第一环节是编解码；
/// - 通过一个结构体同时实现 `InboundHandler` 与 `OutboundHandler`，展示双向管道的写法。
///
/// # How
/// - 记录描述符，按需生成“入站/出站”子描述；
/// - 在入站阶段直接透传消息；
/// - 在出站阶段根据消息类型决定是否立即 flush。
///
/// # 合同
/// - *前置条件*：创建时传入的 `descriptor` 代表逻辑 Handler；
/// - *后置条件*：`on_read` 与 `on_write` 均保持幂等，不改变消息语义。
///
/// # 风险
/// - 错误处理仅打印日志并关闭通道，强调示例优先教学而非完整性。
#[derive(Clone)]
struct CodecHandler {
    descriptor: InitializerDescriptor,
}

impl CodecHandler {
    fn new(descriptor: InitializerDescriptor) -> Self {
        Self { descriptor }
    }

    fn describe_stage(&self, stage: &'static str) -> InitializerDescriptor {
        InitializerDescriptor::new(
            format!("{}.{stage}", self.descriptor.name()),
            self.descriptor.category(),
            format!("{stage} stage of the teaching codec handler"),
        )
    }
}

impl InboundHandler for CodecHandler {
    fn describe(&self) -> InitializerDescriptor {
        self.describe_stage("inbound")
    }

    fn on_channel_active(&self, ctx: &dyn PipelineContext) {
        ctx.logger().info(
            "codec handler activated",
            None,
            Some(ctx.trace_context()),
        );
    }

    fn on_read(&self, ctx: &dyn PipelineContext, msg: PipelineMessage) {
        ctx.forward_read(msg);
    }

    fn on_read_complete(&self, _ctx: &dyn PipelineContext) {}

    fn on_writability_changed(&self, _ctx: &dyn PipelineContext, _is_writable: bool) {}

    fn on_user_event(&self, _ctx: &dyn PipelineContext, _event: spark_core::observability::CoreUserEvent) {}

    fn on_exception_caught(&self, ctx: &dyn PipelineContext, error: CoreError) {
        ctx.logger().error(
            "codec handler encountered error",
            Some(&error),
            Some(ctx.trace_context()),
        );
        ctx.close_graceful(
            CloseReason::new("spark.examples.codec.error", "codec error"),
            None,
        );
    }

    fn on_channel_inactive(&self, ctx: &dyn PipelineContext) {
        ctx.logger().debug(
            "codec handler channel inactive",
            None,
            Some(ctx.trace_context()),
        );
    }
}

impl OutboundHandler for CodecHandler {
    fn describe(&self) -> InitializerDescriptor {
        self.describe_stage("outbound")
    }

    fn on_write(
        &self,
        _ctx: &dyn PipelineContext,
        msg: PipelineMessage,
    ) -> CoreResult<WriteSignal> {
        Ok(if msg.is_user() {
            WriteSignal::AcceptedAndFlushed
        } else {
            WriteSignal::Accepted
        })
    }

    fn on_flush(&self, _ctx: &dyn PipelineContext) -> CoreResult<()> {
        Ok(())
    }

    fn on_close_graceful(
        &self,
        _ctx: &dyn PipelineContext,
        _deadline: Option<core::time::Duration>,
    ) -> CoreResult<()> {
        Ok(())
    }
}

/// `AppRouterHandler`：演示 L2 应用路由阶段。
///
/// # Why
/// - 对握手声明了多路复用能力的连接，需进一步根据帧元数据选择业务路由；
/// - 该 Handler 提醒读者在真实业务中需要解析用户元数据并分发。
///
/// # How
/// - `on_read` 检查消息是否携带用户态元信息，并将其透传；
/// - 其余生命周期钩子保持空实现，聚焦路由关注点。
///
/// # 合同
/// - *前置条件*：消息对象能够提供 `user_kind`；
/// - *后置条件*：所有消息都会继续下发给下游 Handler。
///
/// # 风险
/// - 未实际解析路由键，仅记录日志；真实系统需补充解析与错误处理。
struct AppRouterHandler {
    descriptor: InitializerDescriptor,
}

impl AppRouterHandler {
    fn new(descriptor: InitializerDescriptor) -> Self {
        Self { descriptor }
    }
}

impl InboundHandler for AppRouterHandler {
    fn describe(&self) -> InitializerDescriptor {
        self.descriptor.clone()
    }

    fn on_channel_active(&self, _ctx: &dyn PipelineContext) {}

    fn on_read(&self, ctx: &dyn PipelineContext, msg: PipelineMessage) {
        let metadata_present = msg.user_kind().is_some();
        ctx.logger().debug(
            "app-router inspected inbound metadata",
            None,
            Some(ctx.trace_context()),
        );
        if !metadata_present {
            ctx.logger().warn(
                "app-router received message without user metadata",
                None,
                Some(ctx.trace_context()),
            );
        }
        ctx.forward_read(msg);
    }

    fn on_read_complete(&self, _ctx: &dyn PipelineContext) {}

    fn on_writability_changed(&self, _ctx: &dyn PipelineContext, _is_writable: bool) {}

    fn on_user_event(&self, _ctx: &dyn PipelineContext, _event: spark_core::observability::CoreUserEvent) {}

    fn on_exception_caught(&self, ctx: &dyn PipelineContext, error: CoreError) {
        ctx.logger().error(
            "app-router handler failed",
            Some(&error),
            Some(ctx.trace_context()),
        );
    }

    fn on_channel_inactive(&self, _ctx: &dyn PipelineContext) {}
}

/// `ServiceAdapterHandler`：将 Pipeline 消息引导至业务 Service。
///
/// # Why
/// - L2 之后需要真正的业务处理，Handler 负责在教学示例中展示如何接住请求；
/// - 帮助读者理解 Pipeline Handler 如何与 `BoxService` 协作。
///
/// # How
/// - `on_read` 中记录日志并透传消息，提示在此处可调用 `service`；
/// - 其他钩子提供生命周期日志，强调关闭流程。
///
/// # 合同
/// - *前置条件*：构造函数传入的 `service` 有效且可克隆；
/// - *后置条件*：消息总被传递至下游，未在示例中真正调用 Service。
///
/// # 风险
/// - 未展示异步执行 Service 的流程，避免分散教学重点。
struct ServiceAdapterHandler {
    descriptor: InitializerDescriptor,
    service: BoxService,
}

impl ServiceAdapterHandler {
    /// 构造 Service 适配器，记录描述符与业务 Service。
    ///
    /// # Why
    /// - 在 Handler 生命周期内复用同一 Service；
    /// - 描述符帮助调试器/观测系统识别该阶段。
    ///
    /// # How
    /// - 简单存储参数；
    /// - 不做任何前置调用，等待 `on_read` 时触发业务。
    ///
    /// # 合同
    /// - *参数*：`descriptor` 提供 Handler 元数据；`service` 为业务逻辑入口；
    /// - *返回*：适配器实例。
    ///
    /// # 风险
    /// - 真实环境需考虑 Service 异常、重入与背压，本示例暂不展开。
    fn new(descriptor: InitializerDescriptor, service: BoxService) -> Self {
        Self { descriptor, service }
    }
}

impl InboundHandler for ServiceAdapterHandler {
    fn describe(&self) -> InitializerDescriptor {
        self.descriptor.clone()
    }

    fn on_channel_active(&self, _ctx: &dyn PipelineContext) {}

    fn on_read(&self, ctx: &dyn PipelineContext, msg: PipelineMessage) {
        ctx.logger().info(
            "service adapter received request; forwarding to next handler",
            None,
            Some(ctx.trace_context()),
        );
        ctx.forward_read(msg);
    }

    fn on_read_complete(&self, _ctx: &dyn PipelineContext) {}

    fn on_writability_changed(&self, _ctx: &dyn PipelineContext, _is_writable: bool) {}

    fn on_user_event(&self, _ctx: &dyn PipelineContext, _event: spark_core::observability::CoreUserEvent) {}

    fn on_exception_caught(&self, ctx: &dyn PipelineContext, error: CoreError) {
        ctx.logger().error(
            "service adapter observed pipeline error",
            Some(&error),
            Some(ctx.trace_context()),
        );
    }

    fn on_channel_inactive(&self, ctx: &dyn PipelineContext) {
        ctx.logger().debug(
            "service adapter shutting down",
            None,
            Some(ctx.trace_context()),
        );
    }
}

/// 构造教学用的降级报告对象。
///
/// # Why
/// - `HandshakeOutcome::new` 要求提供降级报告，本函数提供最小实现，避免读者被无关细节干扰；
/// - 通过显式函数强调“降级报告”在握手语义中的位置。
///
/// # How
/// - 返回空能力集的 `DowngradeReport`，表示未发生能力降级。
///
/// # 合同
/// - *前置条件*：无；
/// - *后置条件*：返回值总是表示“未降级”。
fn downgrade_report() -> DowngradeReport {
    DowngradeReport::new(CapabilityBitmap::empty(), CapabilityBitmap::empty())
}

/// 手动构造 `HandshakeOutcome`，便于单元测试验证选择器行为。
///
/// # Why
/// - Selector 闭包依赖握手能力，本函数帮助测试与文档快速造出自定义能力集；
/// - 允许读者在 REPL 或示例中模拟不同能力组合。
///
/// # How
/// - 创建空的能力位图并逐个插入传入能力；
/// - 选用固定版本号 `1.0.0` 与空降级报告；
/// - 调用 `HandshakeOutcome::new` 完成构造。
///
/// # 合同
/// - *参数*：`capabilities` —— 握手能力列表；
/// - *返回*：对应能力位图的 `HandshakeOutcome`；
/// - *前置条件*：能力枚举必须来自 `Capability`；
/// - *后置条件*：返回对象的能力集合恰好覆盖输入集合。
///
/// # 风险
/// - 示例固定版本号，真实系统需根据协议版本动态填写。
pub fn example_handshake(capabilities: &[Capability]) -> HandshakeOutcome {
    let mut bitmap = CapabilityBitmap::empty();
    for capability in capabilities {
        bitmap.insert(*capability);
    }
    HandshakeOutcome::new(Version::new(1, 0, 0), bitmap, downgrade_report())
}

