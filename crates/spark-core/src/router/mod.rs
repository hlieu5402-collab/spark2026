//! 路由子系统的契约集合，面向跨协议、跨平台的通信场景。
//!
//! # 设计蓝图（Why）
//! - **生产级参考**：综合 Envoy、Linkerd、NATS、Apache Pulsar、gRPC 等稳定框架的最佳实践，
//!   强调多维匹配（路径、标签、QoS）、策略可观测与运行时热更新能力，将路由视为独立的控制平面职责。
//! - **学术前沿**：吸收 Intent-Based Networking、Declarative Networking、策略验证 (Policy Verification)
//!   的研究成果，使用结构化 `RoutingIntent` 与 `RouteCatalog` 描述，以便形式化推理与静态分析。
//! - **跨平台要求**：所有契约均基于 `alloc` 友好类型，不依赖 `std`，便于在 IoT、边缘、云原生
//!   等异构环境中复用相同接口。
//!
//! # 模块概览（How）
//! - [`route`]：定义逻辑路由标识与层级结构，兼顾 RPC、消息、流式等多种语义。
//! - [`metadata`]：以类型安全的键值模型表达路由属性，适配治理与流量调度场景。
//! - [`catalog`]：描述可发现的路由条目，为管理平面与观测平台提供统一视图。
//! - [`context`]：将请求、传输意图、观测信息等抽象为路由判定上下文。
//! - [`binding`]：约束路由决策的产出，包括选定的服务实例与附带属性。
//! - [`traits`]：定义泛型层 [`Router`] 与对象层 [`DynRouter`] 的双层契约。
//!
//! # 命名约定（What）
//! - 避免使用 `Spark*` 前缀，选择业界广泛认同的术语（`RouteId`、`RoutingContext` 等）。
//! - 禁用非共识缩写，除 QoS、TTL 等行业惯用语外全部写全。
//! - 模块拆分遵循“单一职责”，保证单文件便于审阅与复用。

pub mod binding;
pub mod catalog;
pub mod context;
pub mod metadata;
pub mod route;
pub mod traits;

pub use binding::{RouteBinding, RouteDecision, RouteValidation};
pub use catalog::{RouteCatalog, RouteDescriptor};
pub use context::{RoutingContext, RoutingIntent, RoutingSnapshot};
pub use metadata::{MetadataKey, MetadataValue, RouteMetadata};
pub use route::{RouteId, RouteKind, RoutePattern, RouteSegment};
pub use traits::{
    DynRouter, RouteBindingObject, RouteDecisionObject, RouteError, Router, RouterObject,
};
