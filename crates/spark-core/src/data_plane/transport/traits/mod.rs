//! Transport 双层接口模块，提供泛型与对象层的互转适配器。

pub mod generic;
pub mod object;

pub use super::server_channel::ServerChannel;
pub use generic::TransportFactory;
pub use object::{
    DynServerChannel, DynTransportFactory, ServerChannelObject, TransportFactoryObject,
};
