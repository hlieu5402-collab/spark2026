//! Pipeline 双层接口模块，暴露泛型层与对象层的互操作入口。

pub mod generic;
pub mod object;

pub use generic::ControllerFactory;
pub use object::{
    ControllerFactoryObject, ControllerHandle, DynControllerFactory, DynControllerFactoryAdapter,
};
