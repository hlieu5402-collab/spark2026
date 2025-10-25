//! 请求解析逻辑。
//!
//! ## 模块目的（Why）
//! - 将 RFC 3261 §7.1 描述的请求行与后续头部解析为零拷贝结构。
//! - 为 `spark-impl-tck` 提供对请求解析的核心能力，便于后续针对 `rport` 等字段做断言。
//!
//! ## 关键流程（How）
//! 1. 切分首行并校验 `SIP/2.0` 版本；
//! 2. 调用 [`parse_headers`](super::headers::parse_headers) 解析头部；
//! 3. 将主体部分按字节切片返回，避免复制。

use crate::{
    error::SipParseError,
    types::{Method, RequestLine, SipMessage, StartLine},
};

use super::{parse_headers, parse_sip_uri, split_first_line, split_headers_body};

/// 解析 SIP 请求文本。
///
/// ### 设计动机（Why）
/// - 将文本转换为 [`SipMessage`] 结构，方便后续业务逻辑基于类型系统进行处理。
///
/// ### 契约说明（What）
/// - **输入**：`input` 必须是包含 CRLF 行结尾的完整请求报文。
/// - **返回**：成功时返回零拷贝的 [`SipMessage`]；失败时给出具体的 [`SipParseError`]。
/// - **前置条件**：文本需符合 RFC 3261 §7.1 格式；
/// - **后置条件**：返回的 `SipMessage` 中所有切片引用原始 `input` 的生命周期。
///
/// ### 实现细节（How）
/// - 使用 `split_first_line` 与 `split_headers_body` 切分文本；
/// - 请求行解析完成后委托 `parse_headers` 处理折行与核心头部；
/// - body 以 `&[u8]` 形式返回，保留原始编码。
pub fn parse_request<'a>(input: &'a str) -> Result<SipMessage<'a>, SipParseError> {
    let (line, rest) = split_first_line(input)?;
    let request_line = parse_request_line(line)?;
    let (header_block, body_block) = split_headers_body(rest)?;
    let headers = parse_headers(header_block)?;
    Ok(SipMessage {
        start_line: StartLine::Request(request_line),
        headers,
        body: body_block.as_bytes(),
    })
}

fn parse_request_line<'a>(line: &'a str) -> Result<RequestLine<'a>, SipParseError> {
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or(SipParseError::InvalidRequestLine)?;
    let uri_text = parts.next().ok_or(SipParseError::InvalidRequestLine)?;
    let version = parts.next().ok_or(SipParseError::InvalidRequestLine)?;

    if !version.eq_ignore_ascii_case("SIP/2.0") {
        return Err(SipParseError::UnsupportedVersion);
    }

    if parts.next().is_some() {
        return Err(SipParseError::InvalidRequestLine);
    }

    Ok(RequestLine {
        method: Method::from_token(method),
        uri: parse_sip_uri(uri_text)?,
        version,
    })
}
