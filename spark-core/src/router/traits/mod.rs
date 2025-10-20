//! 路由契约的双层抽象：泛型 `Router` 与对象层 `DynRouter`。

pub mod generic;
pub mod object;

pub use generic::{RouteError, Router};
pub use object::{DynRouter, RouteBindingObject, RouteDecisionObject, RouterObject};
