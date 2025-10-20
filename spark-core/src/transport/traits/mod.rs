//! Transport 双层接口模块，提供泛型与对象层的互转适配器。

pub mod generic;
pub mod object;

pub use generic::{ServerTransport, TransportFactory};
pub use object::{
    DynServerTransport, DynTransportFactory, ServerTransportObject, TransportFactoryObject,
};
