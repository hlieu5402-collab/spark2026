#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::result_large_err)]
#![doc = "spark-core: 高性能、协议无关、分布式原生的异步通信框架核心契约。"]
#![doc = ""]
#![doc = "== 兼容性与版本治理 (P1.10) =="]
#![doc = "本 Crate 遵守语义化版本 2.0 (SemVer)。"]
#![doc = "1. 破坏性变更 (Breaking Change): 仅允许在 MAJOR 版本（如 11.x -> 12.0）中引入。"]
#![doc = "2. 弃用 (Deprecation): API 弃用必须至少提前 1 个 MINOR 版本（如 11.2 弃用，11.3 保留，11.4 可移除）公告，并保留运行时告警。"]
#![doc = "3. 契约测试: 任何对 `spark-core` 契约的实现或变更，必须同步更新 `spark-contract-tests` 并确保 100% 通过。"]

extern crate alloc;

pub mod buffer;
pub mod common;
pub mod distributed;
pub mod error;
pub mod future;
pub mod observability;
pub mod pipeline;
pub mod runtime;
pub mod service;
pub mod transport;

pub use buffer::{BufferAllocator, ErasedSparkBuf, ErasedSparkBufMut, PipelineMessage};
pub use common::{Empty, IntoEmpty, Loopback};
pub use distributed::{
    ClusterMembershipProvider, DiscoveryEvent, MembershipEvent, NodeId, NodeInfo, NodeStatus,
    ServiceDiscoveryProvider,
};
pub use error::{ErrorCause, SparkError};
pub use future::{BoxFuture, BoxStream, Stream};
pub use observability::{
    ComponentHealth, CoreUserEvent, Counter, Gauge, HealthCheckProvider, HealthState, Histogram,
    IdleDirection, IdleTimeout, Logger, MetricsProvider, OpsEvent, OpsEventBus, RateDirection,
    RateLimited, TlsInfo, TraceContext,
};
pub use pipeline::{
    Channel, ChannelState, Context, ExtensionsMap, InboundHandler, OutboundHandler, Pipeline,
    PipelineFactory, WriteSignal,
};
pub use runtime::{CoreServices, Executor, SparkRuntime, Timer};
pub use service::{Layer, Service};
pub use transport::{Endpoint, ParamMap, ServerTransport, SparkSocketAddr, TransportFactory};

use alloc::boxed::Box;
use core::fmt;

/// `spark-core` 中所有错误必须实现的 `no_std` 基础 Trait。
///
/// # 设计背景（Why）
/// - `std::error::Error` 在 `no_std` 环境中不可用，因此我们需要一个对象安全、与平台无关的错误抽象来串联底层错误链。
/// - 该 Trait 作为所有错误类型的“最小公共接口”，帮助框架在 `alloc` 场景下完成跨模块错误传递。
///
/// # 逻辑解析（How）
/// - 约束实现者提供 `Debug` 与 `Display`，便于日志与可观测性收集。
/// - 通过 `source` 方法递归返回链路上的上游错误，保持与 `std::error::Error::source` 一致的语义，从而兼容现有生态的错误处理约定。
///
/// # 契约说明（What）
/// - **输入/前置条件**：实现类型必须是 `'static` 生命周期并可安全跨线程共享（若需包装进 `ErrorCause`）。
/// - **返回/后置条件**：`source` 返回的引用生命周期受限于 `self`，以防悬垂引用。
///
/// # 设计取舍与风险（Trade-offs）
/// - 我们没有引入 `Send + Sync` 约束，避免对 `no_std` 设备强加多余负担；需要线程安全时请使用 `ErrorCause` 类型别名。
/// - 调用方需注意：若底层错误不提供 `source`，则错误链会在此处终止，这是设计上允许的边界情况。
pub trait Error: fmt::Debug + fmt::Display {
    /// 返回当前错误的上游来源。
    fn source(&self) -> Option<&(dyn Error + 'static)>;
}

impl<E> Error for Box<E>
where
    E: Error + ?Sized,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        (**self).source()
    }
}
