//! 编解码契约的双层抽象：泛型层与对象层。

pub mod generic;
pub mod object;

pub use generic::Codec;
pub use object::{DynCodec, TypedCodecAdapter};
