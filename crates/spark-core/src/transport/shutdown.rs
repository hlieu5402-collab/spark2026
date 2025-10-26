//! `spark-transport` 中半关闭语义的向后兼容层。
//!
//! # 教案级说明
//! - **Why**：维持既有路径 `spark_core::transport::ShutdownDirection`，同时让真正的定义归属
//!   独立的接口 crate，便于多种传输实现共享。
//! - **How**：直接 re-export [`spark_transport::ShutdownDirection`]；调用方无需更新导入路径。
//! - **What**：不再承载任何实现细节，仅作别名桥接。
//! - **Trade-offs**：新增方向时需先更新 `spark-transport`，再同步本模块，确保文档一致。

pub use spark_transport::ShutdownDirection;
