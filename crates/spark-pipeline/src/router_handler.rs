//! 教案级说明：兼容性 Router Handler 模块
//!
//! - **意图 (Why)**：在目录重构前，`spark-pipeline` 暴露过 `router_handler` 模块供旧版
//!   应用直接引用具体的 Handler 类型。为了避免重构后模块解析失败，我们保留同名
//!   模块并从 `spark-router` 再导出核心类型，确保旧路径仍可编译。
//! - **逻辑 (How)**：模块内部不重新实现逻辑，而是 `pub use` `spark_router::pipeline::ApplicationRouter`
//!   及其兼容别名；这样可以共享 `spark-router` 的完整行为与测试覆盖。
//! - **契约 (What)**：调用方需提供与 `spark_router::pipeline::ApplicationRouter` 相同的上下文和
//!   生命周期约束；本模块不引入额外状态，也不承担初始化职责。
//! - **注意事项 (Trade-offs & Gotchas)**：兼容层牺牲了一部分命名的显式性，但换来路径
//!   稳定性；未来若彻底移除旧接口，应在发布说明中明确告知并引导迁移到新的别名。

pub use spark_router::pipeline::{
    AppRouterHandler, ApplicationRouter, ApplicationRouterInitializer, RouterHandler,
};
