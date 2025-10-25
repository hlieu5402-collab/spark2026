//! WebSocket ↔ SIP 映射模块入口。
//!
//! # 教案级概览（Why / How / What）
//! - **定位**：汇聚 RFC 6455 与 RFC 7118 交叉要求下的帧编解码辅助工具，供 TCK 与后续
//!   传输实现共享复用。
//! - **结构**：当前提供 `frame_text_binary` 子模块，聚焦于将文本/二进制数据帧聚合为 SIP
//!   文本报文，并支持逆向拆分。
//! - **扩展**：未来若需支持 MESSAGE summary、心跳或压缩等变体，可在本目录追加子模块，
//!   并在此处集中导出。

pub mod frame_text_binary;
