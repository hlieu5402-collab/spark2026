# Spark 全局架构设计蓝图（装配时 / 运行时双层版）

## 1. 愿景与设计原则
- **装配 / 运行时解耦**：以 `ServerChannel` 握手层（L1）与 `Channel` 运行层（L2）协同，将协议协商、Pipeline 装配与请求调度分离，确保同一套 Handler/Service 可在多协议监听器下复用。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L9-L122】【F:crates/spark-core/src/data_plane/transport/channel.rs†L130-L210】
- **Pipeline 契约中心化**：`PipelineInitializer`、`Pipeline` 与 `Handler` 形成“声明式装配 + 事件驱动执行”的骨架，上层策略与观测统一落在同一套 API 上。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L64-L133】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L1-L200】【F:crates/spark-core/src/data_plane/pipeline/handler.rs†L10-L148】
- **服务访问收敛**：所有业务调用仍通过 Tower 风格的 `Service` 契约，背压、取消与预算语义与 Pipeline 上下文保持一致，确保运行时链路不受装配层变化影响。【F:crates/spark-core/src/data_plane/service/generic.rs†L6-L74】
- **可观测性第一公民**：`InitializerDescriptor`、`PipelineEvent` 与 Router Handler 的上下文快照协同，构建从 L1 协商到 L2 路由的完整追踪链路。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L7-L61】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L56-L121】【F:crates/spark-router/src/pipeline.rs†L26-L211】

## 2. 双层运行模型总览
Spark 的数据面拆分为两个互补层级：

### 2.1 装配时（L1）链路
```
ServerChannel (握手/监听)
  │  └─ set_initializer_selector(HandshakeOutcome → PipelineInitializer)
  ▼
PipelineInitializer (装配策略)
  │  ├─ register_inbound/register_outbound
  │  └─ descriptor() → 观测元数据
  ▼
Pipeline 控制器（生成 Handler Registry + PipelineEvent 广播）
```
- L1 层以 `ServerChannel::set_initializer_selector` 为切入点，根据协商结果挑选合适的 `PipelineInitializer`，继而构造 Pipeline 并登记 Handler 元数据。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L28-L114】【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L64-L133】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】
- 控制面在此阶段注入安全、编解码、观测等横切 Handler，实现部署时的策略冻结；运行时只读取已经编排好的 Handler 队列，无需重新解析配置。

### 2.2 运行时（L2）链路
```
Channel (连接级 IO)
  │  └─ read/write/flush/backpressure
  ▼
Pipeline (事件循环)
  │  └─ 调度 Inbound/Outbound Handler，广播 PipelineEvent
  ▼
Handlers
  │  └─ 普通 Handler（鉴权/编解码/限流/...）
  │  └─ ApplicationRouter (L2 路由 Handler)
  ▼
Service (Tower Service / BoxService)
```
- L2 层在 `Channel` 上驱动事件，Pipeline 控制器根据 Handler 注册表顺序执行入站/出站逻辑，并向下游 Handler 广播事件。【F:crates/spark-core/src/data_plane/transport/channel.rs†L146-L210】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】
- 当请求抵达链路尾端时，由 `ApplicationRouter` 这种 L2 Handler 决定具体服务绑定，并将请求托付给对象层 Router / Service，再将响应写回同一 Channel。【F:crates/spark-router/src/pipeline.rs†L214-L376】

## 3. 生命周期一体化流程
| 阶段 | 关键目标 | 输入 | 输出 | 主要角色 | 核心 API |
| --- | --- | --- | --- | --- | --- |
| 需求立项 | 明确业务域、流量模式、合规限制 | 业务需求、SLO、预估流量 | 架构决策记录、初始 `RoutingIntent` 草案 | 架构师、产品 | `POST /v1/intents` |
| 架构设计 | 拆分 L1/L2 职责，挑选 Handler/Service 契约 | 决策记录、跨领域约束 | `PipelineTemplate`、`InitializerDescriptor` 草案 | 架构师、平台 | `POST /v1/pipelines/templates` |
| 开发实现 | 交付 PipelineInitializer、Handler、Service、Router Handler 实现并自测 | 模板、Mock 数据 | 可部署 Artifact、`ServiceDescriptor` | 开发、QA | `GET /v1/pipelines/templates/{id}`、`PUT /v1/services/{serviceId}` |
| 集成测试 | 验证 L1 协商 + L2 路由链路、观测指标、回滚策略 | Artifact、测试计划 | 测试报告、基线指标、回滚方案 | QA、SRE | `POST /v1/test-suites/run` |
| 部署运营 | 将新的 PipelineInitializer 与 Handler 发布到 ServerChannel，滚动升级 | 测试报告、部署策略 | 生效的监听器、Pipeline 注册表变更 | SRE、平台 | `POST /v1/deployments`、`PATCH /v1/pipelines/{id}:activate` |
| 观测优化 | 采集握手成功率、PipelineEvent、路由耗时 | 遥测数据、`PipelineEvent` 流 | 性能优化建议、异常告警 | 平台、研发 | `GET /v1/telemetry/events`、`GET /v1/pipelines/{id}/registry` |
| 弹性扩缩 | 基于 Channel 背压与服务就绪度动态扩缩资源 | 观测数据、弹性策略 | 更新的资源配置、调度记录 | 平台、SRE | `POST /v1/scale-plans` |
| 退役封存 | 下线 PipelineInitializer/Handler/Service 并归档路由快照 | 归档计划、依赖清单 | 退役报告、配置快照 | 架构师、SRE | `POST /v1/pipelines/{id}:decommission` |

> **说明**：生命周期中所有配置仍通过统一控制面 API 交付，L1/L2 的分离不会改变外部治理入口，部署后运行时仅消耗既定的 Pipeline 装配结果。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L64-L133】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】

## 4. 装配时（L1）工作流细化
1. **监听器启动**：平台调用 `TransportFactory::bind_dyn` 创建 `ServerChannel`，并为其注入协商参数、监听地址等配置；随后设置 `PipelineInitializerSelector`，完成 L1 策略注入。【F:tools/baselines/spark-core.public-api.json†L500-L509】【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L28-L114】
2. **协商完成**：`ServerChannel::accept` 处理握手，输出 `(Channel, TransportSocketAddr)` 与 `HandshakeOutcome`。监听器根据 outcome 选择具体 `PipelineInitializer` 并初始化 Pipeline 控制器。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L61-L114】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】
3. **Handler 编排**：`PipelineInitializer::configure` 使用 `ChainBuilder` 注册入站/出站 Handler，构建最终的 Handler Registry 并生成 introspection 快照，供观测面查询。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L64-L133】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L123-L200】
4. **部署回滚**：若配置冲突或装配失败，控制面依据 `CoreError` 分类执行回滚，并通过事件总线广播失败信息，确保热更新流程可追踪。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L95-L133】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L182-L200】

## 5. 运行时（L2）工作流细化
1. **事件驱动循环**：`Channel` 提供读写、背压和半关闭语义，Pipeline 监听 Channel 事件并驱动入站 Handler 执行；写路径由出站 Handler 协调批处理与刷新。【F:crates/spark-core/src/data_plane/transport/channel.rs†L146-L210】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】【F:crates/spark-core/src/data_plane/pipeline/handler.rs†L10-L92】
2. **扩展上下文**：业务或平台组件可通过 `ExtensionsMap` 写入 `RouterContextState`，为 L2 路由 Handler 提供意图、连接元数据与动态标签。【F:crates/spark-router/src/pipeline.rs†L26-L211】
3. **L2 路由决策**：`ApplicationRouter` 读取上下文、调用 `DynRouter` 完成路由判断，选取 `BoxService`，并将请求托付给运行时执行器，最终写回响应。【F:crates/spark-router/src/pipeline.rs†L214-L376】
4. **服务调用与背压**：被选中的 `Service` 按 `poll_ready`/`call` 协议执行，若返回背压信号，`ApplicationRouter` 结合 Pipeline 事件通知上游执行重试或限流策略。【F:crates/spark-core/src/data_plane/service/generic.rs†L6-L74】【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L56-L121】
5. **观测回路**：Pipeline 持续广播 `PipelineEventKind`，并暴露 Handler Registry 快照，帮助运维侧识别异常链路和时延热点。【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L56-L200】

## 6. 组件协作关系
### 6.1 L1：ServerChannel、PipelineInitializer、Pipeline
- **ServerChannel**：统一监听抽象，负责 L1 协商、PipelineInitializer 选择以及监听器生命周期管理。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L31-L122】
- **PipelineInitializer**：负责声明式装配 Handler，输出 `InitializerDescriptor` 供监控使用，并保证幂等性与线程安全。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L64-L133】
- **Pipeline 控制器**：维护 Handler Registry、事件广播与热插拔能力，是运行时 introspection 的核心入口。【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】

### 6.2 L2：Channel、Pipeline、Handler、ApplicationRouter、Service
- **Channel**：提供连接级 IO 与背压判定，确保 Pipeline 能以统一语义调度不同协议实现。【F:crates/spark-core/src/data_plane/transport/channel.rs†L146-L210】
- **Pipeline**：在运行时调度入站/出站 Handler，并保证事件顺序一致、异常可控。【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L47-L200】
- **Handler**：承担鉴权、编解码、观测等横切逻辑，保持与 Pipeline 上下文的零拷贝交互。【F:crates/spark-core/src/data_plane/pipeline/handler.rs†L10-L148】
- **ApplicationRouter（L2 Handler）**：在 Handler 链尾部执行路由决策，将请求映射到对象层 Service 并驱动异步调用，是旧 Router 逻辑在 L2 的延续。【F:crates/spark-router/src/pipeline.rs†L214-L376】
- **Service**：实现业务调用契约，结合 `CallContext` 提供取消、预算与背压反馈，支撑多语言/多运行时扩展。【F:crates/spark-core/src/data_plane/service/generic.rs†L6-L74】

## 7. API 契约设计
### 7.1 开发体验
- **资源模型**：ServerChannel、PipelineInitializer、Handler、Service、Router Handler 以 RESTful 资源管理，支持标准 CRUD 与版本控制，便于脚本化与 SDK 生成。
- **契约校验**：`POST /v1/contracts/validate` 支持校验 PipelineInitializer 与 Handler 组合，确保装配时不破坏 L2 运行语义。
- **仿真测试**：`POST /v1/simulations/run` 可模拟握手、装配与运行时事件，提前发现背压或路由瓶颈。
- **版本管理**：所有资源暴露 `version` 与 `compat` 字段，配合 `If-Match`/`If-None-Match` 避免多人修改冲突。

### 7.2 运维体验
- **声明式部署**：部署 API 支持一次性推送 ServerChannel + PipelineInitializer + Handler 版本，平台根据 Listener 拓扑自动热更新，确保新旧链路平滑过渡。
- **可观测性**：`GET /v1/telemetry/events` 与 `GET /v1/pipelines/{id}/registry` 展示 PipelineEvent 与 Handler Registry 快照，帮助定位 L1/L2 问题。【F:crates/spark-core/src/data_plane/pipeline/pipeline.rs†L56-L200】
- **回滚与审计**：结合 `InitializerDescriptor` 元数据与部署审计日志，快速识别变更影响范围，支撑安全回滚。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L7-L61】
- **弹性策略**：扩缩 API 根据 Channel 背压与 Service 就绪度自动调整 Listener 实例数或执行器配额。

### 7.3 测试体验
- **环境隔离**：API 支持 `X-Spark-Stage` 头与 `CoreServices` Profile 的组合，保证沙箱/预发/生产隔离。
- **可编排测试流**：集成测试可模拟 L1 握手与 L2 请求链路，验证 PipelineInitializer 与 ApplicationRouter 的组合。
- **断言与覆盖率**：测试报告输出 Handler 调用路径、路由命中率与背压统计，确保链路在各种流量模式下稳定。

## 8. 风险与治理策略
- **协商失败**：若 `ServerChannel` 未能从 `HandshakeOutcome` 选出匹配的 PipelineInitializer，应记录结构化错误并触发自动降级到安全模板，防止监听器停摆。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L28-L114】
- **装配冲突**：`PipelineInitializer` 幂等性不足会导致 Handler 重复注册，需通过契约校验与回滚机制监控并快速回退。【F:crates/spark-core/src/data_plane/pipeline/initializer.rs†L95-L133】
- **运行时背压**：`Channel::classify_backpressure` 与 `Service::poll_ready` 信号需纳入弹性策略，避免热点连接拖垮整个 Listener。【F:crates/spark-core/src/data_plane/transport/channel.rs†L146-L210】【F:crates/spark-core/src/data_plane/service/generic.rs†L6-L74】
- **路由异常**：ApplicationRouter 在构建上下文或调用服务失败时会记录 ERROR 并终止请求，需要配合观测告警与灰度策略快速响应。【F:crates/spark-router/src/pipeline.rs†L214-L376】
- **安全合规**：鉴权、审计、加密 Handler 应在 PipelineInitializer 中以显式顺序声明，并与 L2 Handler 的上下文共享最小必要信息，避免数据泄露。

## 9. 实施建议
1. **分阶段落地**：优先完成 ServerChannel + PipelineInitializer Selector 的改造，确保所有监听器都能在 L1 选择合适的 Pipeline；随后迁移 ApplicationRouter 以适配新的 L2 流程。【F:crates/spark-core/src/data_plane/transport/server_channel.rs†L28-L114】【F:crates/spark-router/src/pipeline.rs†L214-L376】
2. **平台化支持**：提供 CLI/SDK 帮助团队生成 PipelineInitializer 模板、部署 ServerChannel 策略与监控仪表板，降低 L1/L2 分层后的接入成本。
3. **知识沉淀**：建设 Handler/Service/ApplicationRouter 的最佳实践手册与性能基准，尤其关注握手协商成功率、路由命中率与背压信号处理。
4. **生态开放**：继续开放 PipelineInitializer、Handler、Router Handler 的扩展点，鼓励外部团队贡献协议适配器或策略插件，强化生态活力。
