//! 运行时语法糖模块，聚焦在不破坏显式契约的前提下降低调用样板。
//!
//! # 模块定位（Why）
//! - 框架要求在跨组件调用时显式传递 [`CallContext`](crate::contract::CallContext) 以确保取消/截止/预算三元组不丢失。
//! - 业务代码在实际操作中仍需频繁地将上下文与运行时能力组合，容易出现重复样板或遗漏克隆。
//! - 本模块提供轻量包装与宏辅助，统一上下文与运行时的绑定方式，同时保留原有契约接口。
//!
//! # 提供能力（How）
//! - [`sugar::CallContext`]：以零拷贝方式将 `CallContext` 与运行时能力视图绑在一起；
//! - [`sugar::spawn_in`]：基于语法糖上下文调用 [`TaskExecutor::spawn`](crate::runtime::TaskExecutor::spawn)，自动回填 Join 句柄类型；
//! - [`crate::with_ctx!`] 宏：在事件驱动上下文中快速获得语法糖上下文，便于直接调用 [`spawn_in`];
//! - [`sugar::RuntimeCaps`]：统一抽象“可提交任务的运行时能力集合”，支持 `CoreServices`、自定义执行器或 Pipeline `Context`。
//!
//! # 使用契约（What）
//! - 模块仅提供包装，不会隐式缓存全局状态；调用方仍需根据业务语义显式克隆或派生子上下文；
//! - `RuntimeCaps` 实现者应保证底层执行器在任务提交期间有效，且返回的 Join 句柄遵循 [`TaskHandle`](crate::runtime::TaskHandle) 契约；
//! - `with_ctx!` 只在宏作用域内重绑定变量名，不会泄露到外层，避免破坏原始上下文所有权。
//!
//! # 风险提示（Trade-offs & Gotchas）
//! - 若运行时实现返回自定义 Join 句柄，可在 [`RuntimeCaps::spawn_with`] 中自行转换；默认返回
//!   [`crate::runtime::JoinHandle`]；
//! - 语法糖上下文在 `async move` 闭包中使用时，需要显式调用 [`sugar::CallContext::clone_call`] 将上下文克隆为拥有所有权的版本；
//! - `with_ctx!` 假定传入对象实现 [`crate::pipeline::Context`]，若用于其它类型请手动构造 [`sugar::CallContext`]。

pub mod sugar;

pub use sugar::{CallContext, ContextCaps, RuntimeCaps, spawn_in};
