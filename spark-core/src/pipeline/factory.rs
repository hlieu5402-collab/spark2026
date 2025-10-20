//! Pipeline 工厂模块保留向后兼容入口，实际实现迁移至 [`crate::pipeline::traits`]。
//!
//! - 若需要零虚分派的泛型接口，请使用 [`crate::pipeline::traits::generic::ControllerFactory`];
//! - 若运行时需要对象层适配，请参考 [`crate::pipeline::traits::object::DynControllerFactory`] 及相关适配器。

pub use crate::pipeline::traits::generic::ControllerFactory;
pub use crate::pipeline::traits::object::{
    ControllerFactoryObject, ControllerHandle, DynControllerFactory, DynControllerFactoryAdapter,
};
