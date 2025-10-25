//! 错误类型模块。
//!
//! ## 模块目的（Why）
//! - 将解析/格式化阶段的所有失败分门别类，便于调用方通过模式匹配采取补救措施。
//! - 统一错误展示文案，配合 `Display`/`Error` trait，方便在 `anyhow` 等错误栈中串联。
//!
//! ## 使用契约（What）
//! - 解析相关 API 返回 [`SipParseError`]；格式化相关 API 返回 [`SipFormatError`]。
//! - 错误枚举不携带对输入缓冲的引用，避免生命周期难题，且可在日志中安全复制。
//!
//! ## 实现策略（How）
//! - `SipParseError` 聚焦于语法层失败，按照起始行、头部、URI 等分类，便于快速定位。
//! - `SipFormatError` 目前仅涵盖写入失败，未来扩展复杂格式化逻辑时可继续细分。
//!
//! ## 风险提示（Trade-offs）
//! - 若未来需要记录原始错误上下文（例如具体的 header 名称），需权衡与 no_std 场景下分配的取舍。
//! - 当前实现偏重于分类而非详细定位，调用方若需精确位置，可在更高层自行补充。

use core::fmt;

/// SIP 解析阶段可能出现的错误枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipParseError {
    /// 请求行不符合 `METHOD SP URI SP SIP/2.0` 格式。
    InvalidRequestLine,
    /// 响应行不符合 `SIP/2.0 SP Status-Code SP Reason-Phrase` 格式。
    InvalidStatusLine,
    /// 版本号既非 `SIP/2.0` 也不符合兼容要求。
    UnsupportedVersion,
    /// URI 文本无法根据 §19 解析。
    InvalidUri,
    /// 头部名称存在非法字符或缺失冒号。
    InvalidHeaderName,
    /// 头部值为空或违反 §7.3.1 折行规则。
    InvalidHeaderValue,
    /// `Via` 头无法按照 §18.1 解析（如缺失 sent-by）。
    InvalidViaHeader,
    /// `From`/`To` 头无法解析为 name-addr。
    InvalidNameAddrHeader,
    /// `Call-ID` 头无有效 token。
    InvalidCallId,
    /// `CSeq` 头格式不符合 `<number> SP <method>`。
    InvalidCSeq,
    /// `Max-Forwards` 头不是合法的 32 位整数。
    InvalidMaxForwards,
    /// `Contact` 头缺少 URI 或参数格式错误。
    InvalidContact,
    /// 在 header 区域前过早遇到 EOF 或 CRLF 分隔缺失。
    UnexpectedEof,
}

impl fmt::Display for SipParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequestLine => {
                write!(f, "请求行格式错误，期望 `METHOD SP URI SP SIP/2.0`")
            }
            Self::InvalidStatusLine => {
                write!(f, "状态行格式错误，期望 `SIP/2.0 SP Status-Code SP Reason`")
            }
            Self::UnsupportedVersion => write!(f, "仅支持 SIP/2.0 版本号"),
            Self::InvalidUri => write!(f, "URI 解析失败，不符合 RFC 3261 §19"),
            Self::InvalidHeaderName => write!(f, "头部名称非法或缺失冒号"),
            Self::InvalidHeaderValue => write!(f, "头部值违反 RFC 3261 §7.3.1 约束"),
            Self::InvalidViaHeader => write!(f, "Via 头部无法按照 RFC 3261 §18.1 解析"),
            Self::InvalidNameAddrHeader => write!(f, "From/To 头部不是合法的 name-addr 形式"),
            Self::InvalidCallId => write!(f, "Call-ID 头部缺少有效 token"),
            Self::InvalidCSeq => write!(f, "CSeq 头部格式错误，需形如 `<number> <method>`"),
            Self::InvalidMaxForwards => write!(f, "Max-Forwards 需为 0-2^32-1 范围内整数"),
            Self::InvalidContact => write!(f, "Contact 头部缺少 URI 或参数非法"),
            Self::UnexpectedEof => write!(f, "头部或报文意外结束"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SipParseError {}

/// SIP 文本序列化阶段的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SipFormatError {
    /// 底层 `fmt::Write` 写入失败。
    Io,
    /// body 非 UTF-8 编码，当前写入器仅支持文本输出。
    NonUtf8Body,
}

impl fmt::Display for SipFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => write!(f, "写入器返回错误，序列化失败"),
            Self::NonUtf8Body => write!(f, "body 不是合法的 UTF-8 文本"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SipFormatError {}

impl From<fmt::Error> for SipFormatError {
    fn from(_: fmt::Error) -> Self {
        Self::Io
    }
}

impl From<core::str::Utf8Error> for SipFormatError {
    fn from(_: core::str::Utf8Error) -> Self {
        Self::NonUtf8Body
    }
}
