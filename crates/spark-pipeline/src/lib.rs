#![cfg_attr(not(feature = "std"), no_std)]

//! `spark-pipeline` 聚焦提供稳定的 Pipeline Controller 入口。
//!
//! # 教案式说明
//! - **意图（Why）**：将 `spark-core` 中的热插拔控制器打包为独立 Crate，供业务侧或科研实验在不直接依赖
//!   内核实现细节的情况下复用高性能调度能力。
//! - **逻辑（How）**：通过类型别名与再导出，将 `HotSwapController` 暴露为 `PipelineController`，并保留原有
//!   API/特性开关；调用者只需在此 Crate 中构造控制器，即可获得完整的 Handler 管理与 Middleware 装配能力。
//! - **契约（What）**：Crate 默认启用 `alloc` 特性，可在 `no_std` 环境中运行；若启用 `std` 特性，将自动联动
//!   `spark-core/std` 以提供更丰富的运行时支持。
//! - **风险与权衡（Trade-offs）**：复用核心实现意味着链路语义与 `spark-core` 同步更新；若需实验性控制器，
//!   建议在上层引入新类型并显式隔离。

extern crate alloc;

pub use spark_core::pipeline::{Controller, controller::HotSwapController};

mod factory;

pub use factory::DefaultControllerFactory;


mod router_handler;

pub mod router {
    //! `spark-router` 中 Pipeline 集成模块的再导出包装。
    //!
    //! # 教案式说明
    //! - **意图（Why）**：保持原 `spark-pipeline::router` 命名空间不变，便于现有调用方在迁移至
    //!   `spark-router` 统一实现后无需修改路径；
    //! - **逻辑（How）**：简单地将 `spark_router::pipeline` 全量再导出，保证类型与函数与原有
    //!   模块一一对应；
    //! - **契约（What）**：模块内所有符号均直接映射到 `spark-router` 提供的实现，包含上下文
    //!   存取、路由处理器以及构造器接口；
    //! - **风险提示（Trade-offs）**：再导出不会引入额外开销，但要求 `spark-router` 版本与当前
    //!   Crate 同步更新；若未来出现破坏性变更，应优先在 `spark-router` 中维护兼容层。
    pub use spark_router::pipeline::*;
}

pub use spark_router::pipeline::{
    ExtensionsRoutingContextBuilder, RouterContextSnapshot, RouterContextState, RouterHandler,
    RoutingContextBuilder, RoutingContextParts, load_router_context, store_router_context,
};

/// `PipelineController` 是 `spark-pipeline` 对外推荐的默认控制器实现。
///
/// # 教案式说明
/// - **意图（Why）**：通过别名保持 API 的语义化命名，避免直接暴露 `HotSwapController` 的实现细节，
///   便于未来在不破坏调用方代码的情况下迁移到其他热插拔方案。
/// - **逻辑（How）**：类型等价于 [`HotSwapController`]，调用 `PipelineController::new` 时实质上使用
///   核心实现构造函数，并享有其链路管理能力与可观测性集成。
/// - **契约（What）**：构造参数、热插拔操作、事件广播等行为全部继承自 `HotSwapController`；
///   调用方应确保以 `Arc` 持有实例，遵循 `Controller` Trait 的生命周期约束。
/// - **风险与权衡（Trade-offs）**：别名不会产生额外的 ABI 或性能开销，但也意味着该 Crate 与核心实现
///   紧密耦合；若未来需要差异化功能，应考虑新增显式类型而非修改别名。
pub type PipelineController = HotSwapController;
