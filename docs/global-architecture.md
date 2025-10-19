# Spark 全局架构设计蓝图

## 1. 愿景与设计原则
- **研究与工程并重**：以 `spark-core` 的 `Controller`、`Handler`、`Middleware` 契约为核心，既能满足生产级稳定性，也能快速迭代科研算法原型。【F:spark-core/src/pipeline/controller.rs†L68-L118】【F:spark-core/src/pipeline/handler.rs†L8-L83】【F:spark-core/src/pipeline/middleware.rs†L8-L102】
- **全生命周期治理**：设计覆盖需求立项、开发测试、部署运营、观测优化、弹性扩缩以及退役封存，确保各阶段有明确输入输出与责任人。
- **API 契约优先**：所有控制面交互通过稳定版本化 API 暴露，并保证开发、运维、测试三类用户的协作体验。
- **极致可观测与可操作性**：借助 `RouteCatalog`、`RoutingContext` 等路由元信息，结合 `ControllerEvent` 广播机制，实现系统级自解释能力。【F:spark-core/src/router/mod.rs†L1-L31】【F:spark-core/src/pipeline/controller.rs†L14-L61】

## 2. 生命周期一体化流程
| 阶段 | 关键目标 | 输入 | 输出 | 主要角色 | 核心 API |
| --- | --- | --- | --- | --- | --- |
| 需求立项 | 明确业务域、流量模式、合规限制 | 业务需求、SLO、预估流量 | 架构决策记录、初始 `RoutingIntent` 草案 | 架构师、产品 | `POST /v1/intents` |
| 架构设计 | 拆分数据面/控制面/观测面，定义 Handler/Middleware 组合 | 决策记录、跨领域约束 | `PipelineTemplate`、`RouteCatalog` 草案 | 架构师、平台 | `POST /v1/pipelines/templates` |
| 开发实现 | 交付 Handler、Service、Router 实现，并完成自测 | 模板、Mock 数据 | 可部署的 Artifact、`ServiceDescriptor` | 开发、QA | `GET /v1/pipelines/templates/{id}`、`PUT /v1/services/{serviceId}` |
| 集成测试 | 验证跨模块契约、观测链路与回滚策略 | Artifact、测试计划 | 测试报告、基线指标、回滚方案 | QA、SRE | `POST /v1/test-suites/run` |
| 部署运营 | 部署至多环境，滚动升级 & 灰度 | 测试报告、部署策略 | 实际运行中的实例、变更记录 | SRE、平台 | `POST /v1/deployments`、`PATCH /v1/pipelines/{id}:activate` |
| 观测优化 | 采集指标与事件，识别瓶颈 | 遥测数据、`ControllerEvent` 流 | 性能优化建议、异常告警 | 平台、研发 | `GET /v1/telemetry/events`、`GET /v1/pipelines/{id}/registry` |
| 弹性扩缩 | 基于实时流量或预测自动调节资源 | 观测数据、弹性策略 | 更新后的资源配置、调度记录 | 平台、SRE | `POST /v1/scale-plans` |
| 退役封存 | 下线业务流程，清理资源与合规存档 | 归档计划、依赖清单 | 退役报告、配置快照 | 架构师、SRE | `POST /v1/pipelines/{id}:decommission` |


> **说明**：以上阶段均通过同一套 API 管理，保证配置在控制平面可追溯，且运行时遵循 `CoreServices` 提供的执行环境与资源治理能力。【F:spark-core/src/pipeline/middleware.rs†L69-L102】

## 3. 全业务流程与数据流
1. **入口路由判定**：接入层将请求封装为 `RoutingContext`，通过 `Router` 引擎匹配 `RouteCatalog` 与 `RoutingIntent`，决策目标 `Service` 与 Pipeline 配置。【F:spark-core/src/router/mod.rs†L1-L31】
2. **Pipeline 装配**：控制面读取 `PipelineTemplate`，调用 `Middleware::configure` 向 `Controller` 注入入站/出站 `Handler`，并为每个 Handler 注册元数据。【F:spark-core/src/pipeline/middleware.rs†L45-L102】【F:spark-core/src/pipeline/controller.rs†L68-L118】
3. **运行时事件循环**：`Controller` 广播 `ControllerEventKind`，驱动 `InboundHandler` 与 `OutboundHandler` 执行，期间可通过 `CoreServices` 委派异步任务或资源。【F:spark-core/src/pipeline/controller.rs†L14-L118】【F:spark-core/src/pipeline/handler.rs†L8-L83】
4. **业务服务调用**：在 Handler 链末端，`Service` 抽象负责与业务逻辑交互，遵循 Tower 风格的 `poll_ready` / `call` 模式，保证背压控制与协程调度兼容。【F:spark-core/src/service.rs†L1-L52】
5. **观测与治理**：运行时持续向观测面推送 `ControllerEvent`、Pipeline 注册表快照与路由统计信息，用于仪表板、告警与自动化优化。
6. **回路优化**：根据观测数据触发策略引擎，可动态调整 `RoutingIntent`、重新装配 Middleware、更新 Service 版本或扩缩资源，实现闭环治理。

## 4. Pipeline、Handler、Controller、Middleware、Router 关系
- **Router**：负责请求级决策，选择 `RouteBinding` 并指派 Pipeline 模板。其输出决定后续 Controller 应加载的 Handler 拓扑与 Service 实例。【F:spark-core/src/router/mod.rs†L1-L31】
- **Controller**：在数据面承载 Handler 调度，维护入站/出站链路、事件广播与 Handler Registry，为观测面提供 introspection。【F:spark-core/src/pipeline/controller.rs†L14-L118】
- **Middleware**：声明式组装 Handler，持有 `CoreServices` 上下文以访问运行时能力，负责将策略、编解码、观测等横切关注点注入 Pipeline。【F:spark-core/src/pipeline/middleware.rs†L45-L102】
- **Handler**：细化入站 (`InboundHandler`) 与出站 (`OutboundHandler`) 逻辑，处理业务协议、背压、异常与用户事件，支撑全双工通信。【F:spark-core/src/pipeline/handler.rs†L8-L83】
- **Pipeline**：由 Controller 管理的 Handler 链，既可以静态配置，也支持运行时热装配，确保请求处理路径与响应路径的对称性。

### 4.1 分层协作图（文字版）
```
Router (RouteEngine)
  │  └─ RouteDecision → PipelineTemplateId
  ▼
Controller (per connection/stream)
  │  ├─ Middleware.configure(chain, CoreServices)
  │  │    ├─ register_inbound(label, Handler)
  │  │    └─ register_outbound(label, Handler)
  │  ├─ HandlerRegistry.snapshot()
  │  └─ emit_*() → Handlers
  ▼
Handlers (Inbound/Outbound/Duplex)
  │  ├─ 解码/鉴权/流控/观测
  │  └─ 调用业务 Service
  ▼
Service (Tower Trait)
```

## 5. API 契约设计
### 5.1 对开发友好
- **开放资源模型**：Pipeline、Handler、Service、Route 以 RESTful 资源表示，支持 `GET/POST/PUT/PATCH/DELETE` 全操作，便于脚本化与 SDK 生成。
- **契约校验**：提供 `POST /v1/contracts/validate` 接口，输入 Handler 描述与依赖，返回静态分析结果与兼容性矩阵。
- **仿真测试**：`POST /v1/simulations/run` 支持上传流量样本，返回 Latency/Throughput 分析，帮助开发在提交前识别瓶颈。
- **版本管理**：所有资源附带 `version` 与 `compat` 字段，配合 `If-Match` 头实现乐观锁，避免多人协作冲突。

### 5.2 对运维友好
- **声明式部署**：`POST /v1/deployments` 接受 Git Commit + 模板引用，平台根据 `CoreServices` 的执行环境自动滚动更新，支持 `canaryPercent`、`maxUnavailable` 参数。
- **可观测性 API**：`GET /v1/telemetry/events` 流式返回 `ControllerEvent`，`GET /v1/pipelines/{id}/registry` 返回 Handler 拓扑快照，便于快速排障。【F:spark-core/src/pipeline/controller.rs†L14-L118】
- **回滚与审计**：`POST /v1/deployments/{id}:rollback` 与 `GET /v1/audit/logs` 提供完整审计链，结合 `MiddlewareDescriptor` 元信息快速定位变更影响。【F:spark-core/src/pipeline/middleware.rs†L8-L43】
- **弹性策略管理**：`POST /v1/scale-plans` 支持 CPU、延迟阈值、突发流量预测等条件，结合路由层自动扩缩。

### 5.3 对测试友好
- **环境隔离**：所有 API 支持 `X-Spark-Stage` 头部，自动对接不同的 `CoreServices` Profile，保证沙箱、预发、生产隔离。
- **可编排测试流**：`POST /v1/test-suites/run` 接受 Pipeline Template 与流量脚本，平台使用模拟 Router 与 Controller 执行端到端验证。
- **断言与覆盖率**：`GET /v1/test-suites/{id}/report` 提供 Handler 调用路径覆盖率、事件触发分布，辅助测试评估。
- **契约基准**：`POST /v1/contracts/mock` 自动生成基于 Handler 描述的 Mock Service，减少测试准备成本。

## 6. 行业内标杆对比
| 能力 | Spark 方案 | Envoy | Linkerd | gRPC | 优势总结 |
| --- | --- | --- | --- | --- | --- |
| Pipeline 编排 | 声明式 Middleware + Handler 双向链路，热插拔 | 静态 Filter 链，热更新需 LDS | Service Mesh 层面，功能有限 | 拦截器粒度较粗 | 灵活度与可组合性优于三者 |
| 路由控制 | `RoutingIntent` + `RouteCatalog` 支持多维策略 | RDS 动态路由 | 基于服务发现，策略有限 | 客户端路由，功能弱 | 策略表达力更强 |
| 可观测性 | Handler Registry + ControllerEvent 标准化事件流 | xDS + Stats，但 Handler 级需自扩展 | Tap/metrics，缺细粒度事件 | 拦截器自实现 | 内建粒度细，可直接对标科研需求 |
| 扩展性 | `Service`/`Layer` Trait 适配任意协议、运行时 | C++ 插件 | Rust 限制多 | 语言限定 | Rust Trait 组合，灵活度高 | 
| API 体验 | 统一 `/v1` 资源模型，覆盖 Dev/Ops/Test | Control Plane API 分散 | 以 CLI/CRD 为主 | Proto 接口单一 | 一站式控制面 |

> **综合**：Spark 结合 xDS/Service Mesh 与函数式中间件优势，在灵活度、可观测性与全生命周期治理上可与行业 Top3 并肩甚至超越。

## 7. API 交互体验优化
- **一致的资源路径**：遵循 `/v1/{资源}/{标识}` 命名，提供 `?view=summary|full`、`pageToken` 等查询参数，满足 UI 与脚本需求。
- **幂等与事务保障**：关键写操作均要求客户端提供 `requestId`，服务端实现去重；跨资源更新采用 Saga 模式回滚。
- **实时反馈**：所有长时任务（测试、部署、仿真）返回 `operationId` 并支持 `GET /v1/operations/{id}` 轮询状态，或通过 SSE/WebSocket 订阅进度。
- **智能默认值**：API 根据 `MiddlewareDescriptor.category` 自动推断监控模板、日志采集策略，减少表单填写工作量。
- **强类型 Schema**：公开 OpenAPI + AsyncAPI 文档，并提供 Rust/Go/TypeScript SDK，确保多语言团队使用顺滑。
- **类型安全扩展点**：`PipelineMessage::from_user` 与 `CoreUserEvent::from_application_event` 提供统一的对象安全封装，避免 `Any` 下转型遗漏错误处理。

## 8. 风险与治理策略
- **配置漂移**：通过 `PipelineTemplate` 版本与审计日志对齐环境配置，异常时自动触发比对与回滚。
- **性能瓶颈**：利用仿真测试与实时指标，关注 `on_read`、`on_write` 热路径，必要时提供自动化建议或扩展 Handler 到专用执行器。【F:spark-core/src/pipeline/handler.rs†L24-L83】
- **异常风暴**：Controller 捕获 `ExceptionRaised` 时触发熔断/降级策略，结合 Router 调整流量或重试策略。【F:spark-core/src/pipeline/controller.rs†L14-L61】
- **一致性风险**：跨区域部署通过 `RoutingSnapshot` 下发全局一致视图，并在控制面执行分布式锁或版本门槛。
- **安全合规**：Middleware 层提供鉴权、加密、审计 Hook，可依据行业规范快速装配或更换。

## 9. 实施建议
1. **落地节奏**：优先完成 Router + Pipeline 模板 + 控制面 API MVP，随后补充仿真测试与弹性策略。
2. **平台化支持**：建设 SDK、CLI 与 Terraform Provider，降低团队接入门槛。
3. **知识沉淀**：建立 Handler/Middleware 模板库、最佳实践手册与性能基准，持续提升复用率。
4. **开放生态**：定义插件接口，允许外部团队贡献自定义 Middleware 与 Service，实现生态共建。
