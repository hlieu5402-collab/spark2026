//! 此文件用于保持目录结构与上游规范同步。
//!
//! 当前 `spark-core` 源码位于仓库根目录下的 `spark-core/`，TCK 仍引用
//! `crates/spark-core/src` 路径进行存在性校验。为避免契约测试失败，这里
//! 以 re-export 形式桥接至实际实现。
pub use spark_core::buffer::buf_view::*;
