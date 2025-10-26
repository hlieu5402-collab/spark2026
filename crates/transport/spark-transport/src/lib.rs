#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
#![allow(clippy::result_large_err)]
#![doc = "spark-transport: 传输层契约接口统一抽象层。"]
#![doc = ""]
#![doc = "== 使命概述 =="]
#![doc = "- **Why**：为 Spark 运行时的 TCP/UDP/QUIC/TLS 等实现提供共同语言，确保热插拔替换时无需重编译调用方。"]
#![doc = "- **What**：定义 Listener、Connection、预算/限流/背压型材等核心 trait，并提供 `TransportSocketAddr` 等基础结构。"]
#![doc = "- **How**：面向 `no_std + alloc` 环境设计，所有实现仅需依赖本 crate 即可遵循统一契约。"]

extern crate alloc;

/// `Result` 是传输层契约内部使用的统一返回别名，避免直接依赖 `spark-core` 造成循环引用。
///
/// # 设计背景（Why）
/// - `spark-core` 依赖本 crate 暴露的接口，若在此直接引入 `spark-core::Result` 会导致依赖环。
/// - 通过本地别名确保传输层仍遵循统一错误语义，调用方可以在上层将错误转换为框架标准形式。
///
/// # 使用方式（How）
/// - 与 `core::result::Result` 完全等价，默认不指定错误类型，调用者需在签名中显式声明错误枚举。
/// - 上层若需要转换为 `spark-core::Result`，可在桥接实现中直接 `map_err`。
pub type Result<T, E> = core::result::Result<T, E>;

pub mod addr;
pub mod backpressure;
pub mod budget;
pub mod connection;
pub mod listener;
pub mod rate;
pub mod shutdown;

pub use addr::TransportSocketAddr;
pub use backpressure::{BackpressureClassifier, BackpressureDecision, BackpressureMetrics};
pub use budget::{Budget, BudgetGuard};
pub use connection::{DatagramEndpoint, TransportConnection};
pub use listener::TransportListener;
pub use rate::{RateLimiter, RatePermit};
pub use shutdown::ShutdownDirection;
