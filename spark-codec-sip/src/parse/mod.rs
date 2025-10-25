//! 解析模块入口。
//!
//! ## 模块目标（Why）
//! - 提供请求、响应及头部的统一解析入口，满足 RFC 3261 §7–§8 对报文结构的要求。
//! - 处理 header 折行与大小写匹配，为上层提供语义化结构。
//!
//! ## 结构概览（What）
//! - [`parse_request`]：解析 SIP 请求文本，返回 [`SipMessage`](crate::types::SipMessage)。
//! - [`parse_response`]：解析 SIP 响应文本。
//! - 内部模块 `request`/`response`/`headers` 各司其职，分别处理起始行与头部细节。
//!
//! ## 实现策略（How）
//! - 先切分起始行与 header/body，再委托子模块完成字段级解析；
//! - 全程保持对原始缓冲的引用，避免复制；
//! - 对折行（linear white space）使用统一辅助函数处理。
//!
//! ## 风险提示（Trade-offs）
//! - 当前实现假定输入为 UTF-8；若未来需要处理原始字节流，可增加 `parse_request_bytes` 变体。
//! - 若 header 过多可能导致 `Vec` 扩容，可在性能敏感场景改用自定义分配器。

pub(crate) mod common;
mod headers;
mod request;
mod response;

pub use request::parse_request;
pub use response::parse_response;

pub(crate) use common::{parse_sip_uri, split_first_line, split_headers_body};
pub(crate) use headers::parse_headers;
