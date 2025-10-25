//! Service Trait 分层定义：统一暴露泛型层与对象层接口。

pub mod generic;
pub mod object;

pub use generic::{Layer, Service};
pub use object::{BoxService, DynService, ServiceObject};
