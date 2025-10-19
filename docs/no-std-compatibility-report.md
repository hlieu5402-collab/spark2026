# spark-core 纯 no_std（无 alloc）兼容性可行性评估

> 作者：AI 助手（根据仓库当前提交生成）

## 1. 研究背景与目标
- `spark-core` 当前以 `no_std + alloc` 形态运行，在嵌入式、裸机或极端受限环境下存在移植障碍。
- 本研究聚焦核心契约（Pipeline 事件分发、Handler 接口、Buffer 视图等），评估是否可通过特性开关或并行的替代 Trait 组，提供**无需依赖 `alloc`** 的最小可用子集。
- 输出：识别关键技术挑战、潜在收益、可能的替代设计，以及对既有 API 契约的影响。

## 2. 现状速览
- 缓冲模块广泛使用 `Box<dyn ReadableBuffer>`、`Vec<u8>` 来承载字节与业务消息，直接依赖堆分配。【F:spark-core/src/buffer/message.rs†L1-L114】【F:spark-core/src/buffer/mod.rs†L1-L61】
- Pipeline 控制面需要在注册 Handler、广播事件时动态分配 `Box<dyn Handler>`、`Vec<HandlerRegistration>` 等容器。【F:spark-core/src/pipeline/controller.rs†L1-L123】【F:spark-core/src/pipeline/controller.rs†L151-L214】
- Handler/Context 契约预设 `Send + Sync + 'static` 以及 `PipelineMessage`（内含 Box/Vec）等对象安全需求，对纯 `no_std` 环境的线程模型与内存策略提出挑战。【F:spark-core/src/pipeline/handler.rs†L1-L86】【F:spark-core/src/pipeline/context.rs†L1-L60】
- 顶层文档已将“no_std + alloc”作为硬性约束，未提供纯 `no_std` 变体或特性开关。【F:spark-core/src/lib.rs†L1-L22】

## 3. 核心障碍分析

### 3.1 Pipeline 事件分发
- **对象安全 + 动态装配**：`Controller::register_*` 接口要求 `Box<dyn Handler>`，以便在运行时插拔 Handler。【F:spark-core/src/pipeline/controller.rs†L151-L183】
  - 在无分配器环境中无法直接构造 `Box`；若改用栈分配，注册生命周期与所有权管理将需要全局静态存储或外部内存池。
- **事件快照**：`HandlerRegistry::snapshot` 返回 `Vec<HandlerRegistration>`，暗含复制成本与分配器依赖。【F:spark-core/src/pipeline/controller.rs†L131-L149】
  - 若改用 `&'a [HandlerRegistration]` 视图，需要长期保留链路描述，意味着内部必须具备静态容量或外部存储。

### 3.2 Handler 契约
- Handler trait 要求 `Send + Sync + 'static`，适用于多线程运行时，但在裸机或单核环境中可能只需 `!Send`/`!Sync` 约束。【F:spark-core/src/pipeline/handler.rs†L24-L86】
- `PipelineMessage` 以 `Box<dyn ReadableBuffer>` / `Box<dyn UserMessage>` 承载数据；纯 `no_std` 场景缺乏堆分配，无法直接满足此设计。【F:spark-core/src/buffer/message.rs†L85-L156】
- Handler 回调普遍假定消息可被所有权转移（move），在无分配器时需要零拷贝引用或环形缓冲支持。

### 3.3 Buffer 视图与池化
- `BufferAllocator::acquire` 返回 `Box<ErasedSparkBufMut>`，且默认实现依赖 `BufferPool`，代表必须在堆上租借缓冲。【F:spark-core/src/buffer/mod.rs†L33-L61】
- `PipelineMessage::Bytes` 通过 `Vec<u8>` 暴露轻量快照；无分配器时需替换为固定容量数组或外部提供的 `&mut [u8]` 切片。【F:spark-core/src/buffer/message.rs†L1-L84】
- 缓冲池实现普遍依赖统计结构（如 `Vec`、`Arc`）记录状态，需在无分配器环境中重新设计。

### 3.4 运行时 & 可观测性依赖
- `Context` 暴露执行器、计时器、可观测性等组件，默认指向异步运行时（Tokio 等），这些实现高度依赖 `alloc`。【F:spark-core/src/pipeline/context.rs†L1-L60】
- 即便提供纯同步接口，仍需解决 `CoreServices` 中 Arc/Box 组合的问题（当前实现未列出但在运行时代码中普遍存在）。

## 4. 潜在解决方案与评估

| 方案 | 描述 | 复杂度 | 对契约影响 | 备注 |
| ---- | ---- | ---- | ---- | ---- |
| A. 特性开关拆分 (`alloc` vs. `bare`) | 在 `Cargo.toml` 中新增 `bare` feature，切换到无堆分配实现。通过 `cfg` 提供平行的 Handler/Pipeline/Buffer trait。 | 高 | 引入大量条件编译；调用方需针对两套 trait 编译。 | 需维护双份 API，测试矩阵复杂。 |
| B. 引入泛型化消息/缓冲 | 将 `PipelineMessage` 参数化为 `PipelineMessage<B, U>`，允许在无堆版本中使用切片或静态对象。 | 中 | 破坏现有类型别名，需要调用方泛型迁移。 | 可利用 `type PipelineMessage = PipelineMessage<Box<...>>` 兼容。 |
| C. 外部 Arena Trait | 定义 `Arena`/`Slab` 抽象，由调用者提供静态内存区域；框架以索引或句柄引用对象。 | 高 | Handler 需要改写成“索引传递”模式，弱化对象安全。 | 适合嵌入式，但牺牲 ergonomics。 |
| D. 采用静态容量容器 | 对 Handler 注册表等结构改用 `heapless::Vec` 或 const generics 容量上限。 | 中 | 需要在 API 上携带容量泛型参数；容量不足时需定义错误处理契约。 | 需引入额外依赖或暴露 `const N: usize`。 |
| E. 同步最小子集 | 发布 `spark-core-sync` 子模块，仅暴露同步、无堆接口（例如 `fn on_read(&self, &[u8])`）。 | 中 | 新增子模块 + 文档说明；调用方需自行桥接至异步版本。 | 适合作为实验性路径，逐步覆盖核心功能。 |

综合评估：B + D 的组合能在保持主要契约结构的前提下提供替代实现；A/C 适合长期规划但初期成本高；E 可作为阶段性成果验证市场需求。

## 5. 技术收益与风险

**潜在收益**
- 覆盖嵌入式、边缘推理、网络设备等场景，拓展生态影响力。
- 通过泛型和 const 容量约束，可能带来额外的编译期优化（零成本抽象）。
- 为未来在 `spark-contract-tests` 中增加更多运行时后端打下基础。

**主要风险**
- 维护成本显著上升：对象安全版本与泛型版本需要双向适配测试。
- 调用方迁移成本高：现有依赖 `Box`/`Vec` 的实现需重写。
- API 契约稳定性受影响：若采用特性开关，语义兼容性需要详细版本治理策略。

## 6. 对现有 API 契约的影响
- 需要在文档中明确“当前版本仅支持 `no_std + alloc`，纯 `no_std` 正在评估”，避免误用。【F:spark-core/src/lib.rs†L11-L22】
- 若引入泛型/容量参数：
  - Handler 签名可能从 `PipelineMessage` 变更为 `PipelineMessage<B, U>`，必须提供类型别名保持旧接口。
  - `Controller::register_*` 需支持静态分配 Handler（例如通过 `&'static dyn InboundHandler` 或 `ArenaHandle`）。
  - `HandlerRegistry::snapshot` 需返回借用视图或迭代器，以避免强制分配。
- 对契约测试 (`spark-contract-tests`) 需新增无堆环境配置，验证泛型路径的语义等价性。

## 7. 建议与下一步
1. **文档更新**：在 `lib.rs` 顶级文档说明当前 alloc 依赖，并概述潜在的纯 `no_std` 路线图（泛型消息 + 外部内存提供者）。
2. **原型验证**：创建实验性分支，引入 `PipelineMessage<B>` 泛型与 `heapless` 实现，验证 API 可行性。
3. **契约测试扩展**：为 `spark-contract-tests` 添加 `bare-metal` profile，在 CI 中运行最小功能子集。
4. **生态沟通**：与下游库沟通迁移成本，收集对泛型化的需求反馈，再决定是否进入主线。

## 8. 结论
- 纯 `no_std`（无 alloc）兼容在技术上可行，但涉及广泛 API 重构；建议以实验性 feature + 类型别名兼容的方式渐进推进。
- 在完成泛型与外部内存提供者设计之前，仍需维持 `alloc` 依赖作为默认路径，确保主流异步运行时与生态协同。
