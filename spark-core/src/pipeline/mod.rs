//! Pipeline 模块汇聚跨平台通信框架的控制面契约。
//!
//! ## 设计溯源（Why）
//! - **业界稳健实践**：综合 Netty ChannelPipeline、Envoy FilterChain、gRPC Interceptor、NATS JetStream Consumer、Tower Service Stack 的 API 设计，提炼出适用于 Rust 的控制面抽象。
//! - **前沿探索**：吸收 Actor-Oriented Pipeline、eBPF 数据面编排、可验证中间件（Verifiable Middleware）等研究成果，允许在 Controller 层注入可观测、可验证的增强能力。
//! - **生产与科研兼容**：模块化拆分为 `channel`、`context`、`controller`、`middleware`、`handler` 等子模块，既可最小化编译单元，也方便在科研场景下替换任意环节。
//!
//! ## 模块说明（What）
//! - [`channel`]：面向连接生命周期的统一抽象，强调跨协议一致性。
//! - [`context`]：Handler/Middleware 与运行时交互的核心接口。
//! - [`controller`]：负责事件广播、链路管理与链路遥测。
//! - [`middleware`]：以配置化方式批量装配 Handler 链路，兼容 Layer/Middleware/Interceptor 等模式。
//! - [`handler`]：定义入站、出站以及全双工 Handler 合同。
//! - [`factory`]：为接入层提供 Controller 构造逻辑。
//! - [`extensions`]：统一的类型安全扩展点存储。
//!
//! ## 命名约定（Consistency）
//! - 所有公开组件名称避免携带 `Spark` 前缀，以强调模块的通用性。
//! - 避免非共识缩写，接口命名遵循“动词+名词”或“名词+描述”格式，便于多语言团队理解。

pub mod channel;
pub mod context;
pub mod controller;
pub mod extensions;
pub mod factory;
pub mod handler;
pub mod middleware;
pub mod traits;

pub use channel::{Channel, ChannelState, WriteSignal};
pub use context::Context;
pub use controller::{Controller, ControllerEvent, ControllerEventKind, HandlerRegistry};
pub use extensions::ExtensionsMap;
pub use handler::{DuplexHandler, InboundHandler, OutboundHandler};
pub use middleware::{ChainBuilder, Middleware, MiddlewareDescriptor};
pub use traits::generic::ControllerFactory;
pub use traits::generic::ControllerFactory as PipelineFactory;
pub use traits::object::{
    ControllerFactoryObject, ControllerHandle, DynControllerFactory, DynControllerFactoryAdapter,
};

/// 为兼容旧版 API，保留 `Pipeline` 名称映射至新的 [`Controller`] 概念。
pub use controller::Controller as Pipeline;
