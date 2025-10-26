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
