//! Service 子系统：定义 Spark 数据平面的双层（泛型 + 对象）业务调用契约。
//!
//! # 结构概览
//! - [`traits::generic`]：面向零虚分派场景的泛型接口（`Service`/`Layer`）；
//! - [`traits::object`]：面向插件、脚本与动态加载的对象安全接口（`DynService`/`ServiceObject`/`BoxService`）。
//!
//! # 设计约束
//! - 两层接口在语义上保持等价，均遵守 `CallContext` 统一上下文、背压契约与优雅关闭约定；
//! - 对象层仅依赖最小集合（`PipelineMessage` 等），以降低插件体积并减少编译依赖。

pub mod auto_dyn;
pub mod metrics;
pub mod simple;
pub mod traits;

pub use auto_dyn::{
    AutoDynBridge, Decode, DynBridge, Encode, bridge_to_box_service, type_mismatch_error,
};
pub use metrics::{PayloadDirection, ServiceMetricsHook, ServiceOutcome};
pub use simple::SimpleServiceFn;
pub use traits::generic::{Layer, Service};
pub use traits::object::{BoxService, DynService, ServiceObject};
