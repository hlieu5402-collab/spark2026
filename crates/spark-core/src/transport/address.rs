//! `spark-transport` 中地址抽象的向后兼容层。
//!
//! # 教案级说明
//! - **意图（Why）**：保持历史路径 `spark_core::transport::TransportSocketAddr` 的可用，
//!   同时将权威实现迁移至 `spark-transport`，实现契约与实现解耦。
//! - **架构位置（How）**：该模块仅作 re-export，不再维护任何逻辑；
//!   真正的定义位于 `spark-transport` crate 中，便于传输实现独立复用。
//! - **契约声明（What）**：直接公开 [`spark_transport::TransportSocketAddr`]。
//! - **风险提示（Trade-offs）**：未来新增地址变体时必须先更新 `spark-transport`，
//!   再在此处同步 re-export，确保调用方看到的定义保持一致。

pub use spark_transport::TransportSocketAddr;
