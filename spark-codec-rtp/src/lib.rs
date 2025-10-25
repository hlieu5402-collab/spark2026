#![cfg_attr(not(feature = "std"), no_std)]
#![warn(missing_docs)]

//! # spark-codec-rtp
//!
//! ## 教案目的（Why）
//! - **定位**：Real-time Transport Protocol (RTP) 数据面编解码骨架，负责媒体帧的封包/拆包。
//! - **架构角色**：在整体语音/视频栈中，RTP 紧随 SDP 协商之后运行，对媒体负载进行序列化并携带时序信息。
//! - **设计策略**：本实现交付零拷贝解析、序列回绕比较与基础报文生成三项基线能力，为未来的完整媒体通道实现奠定测试契约。
//!
//! ## 交互契约（What）
//! - **依赖输入**：基于 `spark-core` 的 `BufView` 抽象获取原始字节视图，避免提前复制网络包。
//! - **输出职责**：
//!   1. `RtpHeader`/`RtpPacket` 提供结构化访问与零拷贝数据窗口；
//!   2. `parse_rtp` 将 `BufView` 解析为 `RtpPacket`；
//!   3. `RtpPacketBuilder` 回写基础 RTP 报文，确保 header/扩展/填充一致。
//! - **前置条件**：假设输入缓冲遵循 RFC 3550 字节序与字段排列；调用方需保证在视图生命周期内底层数据保持只读。
//!
//! ## 实现策略（How）
//! - **解析路径**：通过 `ByteSpan` 记录 payload/扩展的逻辑区间，再以 `BufView::as_chunks` 派生零拷贝切片；整个过程中仅对前 12 字节执行栈内复制以完成位域拆解。
//! - **生成路径**：`RtpPacketBuilder` 根据 header 元数据拼装报文，并验证扩展长度、padding 标记等契约，避免产生不合法报文。
//! - **序列比较**：实现 RFC 3550 附录 A 中的半区差分逻辑，确保回绕时序列大小比较保持稳定。
//!
//! ## 风险提示（Trade-offs）
//! - **多分片输入**：当 `BufView` 由多个分片组成时，解析流程会逐片迭代以构建零拷贝窗口，需关注迭代成本；
//! - **扩展与 padding**：生成路径要求调用方显式声明扩展与 padding 的一致性，否则会返回错误；
//! - **后续扩展**：未来若支持一阶 header 扩展 (RFC 8285)，需在保持零拷贝的前提下扩展结构体字段。

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use spark_core::buffer::{BufView, Chunks};

/// RTP 固定版本号（RFC 3550 §5.1）。
pub const RTP_VERSION: u8 = 2;

/// RTP Header 最小长度（单位：字节），即无 CSRC/扩展时的 12 字节定长部分。
pub const RTP_HEADER_MIN_LEN: usize = 12;

/// CSRC 数量上限（4 bit 字段，最大值 15）。
pub const MAX_CSRC_COUNT: usize = 15;

/// RTP Header 的结构化表示，覆盖 RFC 3550 §5.1 规定的所有字段。
///
/// ### Why
/// - 业务逻辑需要以强类型方式读取 RTP 头部字段，例如 payload type、序列号与时间戳。
/// - 当 TCK 解析报文时，可直接对结构体字段断言而无需手动位运算。
///
/// ### What
/// - `version/padding/extension/marker/payload_type` 对应首两个字节的位域；
/// - `sequence_number/timestamp/ssrc` 对应随后的定长字段；
/// - `csrcs` 存储至多 15 个 CSRC 标识符，并以 `csrc_count` 记录有效长度。
///
/// ### How
/// - 解析时将位域拆解写入该结构，生成报文时则按字段重组；
/// - 通过 `set_csrcs` 与 `csrcs` 提供安全的 CSRC 访问接口，避免手动维护长度与数组同步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpHeader {
    /// RTP 版本号，当前仅支持值 2。
    pub version: u8,
    /// Padding 标记，指示 payload 末尾是否附带填充字节。
    pub padding: bool,
    /// Extension 标记，指示 header 后是否紧跟扩展头。
    pub extension: bool,
    /// CSRC 数量（0-15），决定是否需要读取额外的同步源列表。
    pub csrc_count: u8,
    /// Marker 位，用于语义层信号（如帧边界）。
    pub marker: bool,
    /// Payload Type（7 bit），由会话协商决定媒体负载含义。
    pub payload_type: u8,
    /// 序列号（16 bit），每个 RTP 包递增。
    pub sequence_number: u16,
    /// 时间戳（32 bit），标识采样时刻。
    pub timestamp: u32,
    /// 同步源标识符（32 bit）。
    pub ssrc: u32,
    csrcs: [u32; MAX_CSRC_COUNT],
}

impl Default for RtpHeader {
    fn default() -> Self {
        Self {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type: 0,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
            csrcs: [0; MAX_CSRC_COUNT],
        }
    }
}

impl RtpHeader {
    /// 返回当前 header 中的 CSRC 列表。
    #[must_use]
    pub fn csrcs(&self) -> &[u32] {
        &self.csrcs[..self.csrc_count as usize]
    }

    /// 设置 CSRC 列表。
    ///
    /// - **Contract**：输入切片长度不得超过 15；调用后 `csrc_count` 将被更新为切片长度。
    pub fn set_csrcs(&mut self, csrcs: &[u32]) -> Result<(), RtpEncodeError> {
        if csrcs.len() > MAX_CSRC_COUNT {
            return Err(RtpEncodeError::InvalidField("csrc_count"));
        }
        self.csrc_count = csrcs.len() as u8;
        for (idx, value) in csrcs.iter().enumerate() {
            self.csrcs[idx] = *value;
        }
        Ok(())
    }
}

/// RTP Header 扩展视图，保存 profile 标识与零拷贝数据窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeaderExtension {
    /// 扩展头 profile 标识（16 bit）。
    pub profile: u16,
    span: ByteSpan,
}

impl RtpHeaderExtension {
    /// 返回扩展数据的逻辑位置。
    #[must_use]
    pub const fn span(&self) -> ByteSpan {
        self.span
    }
}

/// 表示零拷贝的字节窗口，供 payload / 扩展共享使用。
#[derive(Clone, Copy)]
pub struct RtpByteSection<'a> {
    view: &'a dyn BufView,
    span: ByteSpan,
}

impl<'a> RtpByteSection<'a> {
    /// 从原始 `BufView` 与 `ByteSpan` 构建零拷贝窗口。
    fn new(view: &'a dyn BufView, span: ByteSpan) -> Self {
        Self { view, span }
    }

    /// 返回逻辑长度。
    #[must_use]
    pub fn len(&self) -> usize {
        self.span.len()
    }

    /// 判断窗口是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.span.is_empty()
    }

    /// 将窗口转换为 `Chunks` 迭代器，保持零拷贝语义。
    pub fn as_chunks(&self) -> Chunks<'a> {
        slice_chunks(self.view, self.span)
    }
}

impl fmt::Debug for RtpByteSection<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtpByteSection")
            .field("offset", &self.span.offset)
            .field("len", &self.span.len)
            .finish()
    }
}

/// RTP Header 扩展的零拷贝视图。
#[derive(Debug, Clone, Copy)]
pub struct RtpHeaderExtensionView<'a> {
    /// 扩展 profile 标识。
    pub profile: u16,
    /// 扩展数据窗口。
    pub data: RtpByteSection<'a>,
}

/// 完整的 RTP Packet 结构，包含 header 元数据与零拷贝 payload 视图。
#[derive(Clone)]
pub struct RtpPacket<'a> {
    header: RtpHeader,
    view: &'a dyn BufView,
    payload: ByteSpan,
    extension: Option<RtpHeaderExtension>,
    padding_len: u8,
}

/// 将 `BufView` 解析为 `RtpPacket`，保持 payload/扩展的零拷贝视图。
///
/// # 调用契约
/// - **输入缓冲**：须包含完整的 RTP 报文（含可选 CSRC、扩展与 padding）。
/// - **返回值**：成功时返回携带零拷贝引用的 `RtpPacket`；失败时返回 `RtpParseError`。
/// - **前置条件**：调用期间底层缓冲不可被修改或释放，确保 `RtpPacket` 内的视图有效。
/// - **后置条件**：若成功，`payload()`/`extension()` 均可在不复制底层字节的情况下访问原始数据。
pub fn parse_rtp<'a>(buffer: &'a dyn BufView) -> Result<RtpPacket<'a>, RtpParseError> {
    let view: &'a dyn BufView = buffer;
    let total_len = view.len();
    if total_len < RTP_HEADER_MIN_LEN {
        return Err(RtpParseError::HeaderTooShort);
    }

    let mut fixed = [0u8; RTP_HEADER_MIN_LEN];
    if !read_into(view, 0, &mut fixed) {
        return Err(RtpParseError::HeaderTooShort);
    }

    let version = fixed[0] >> 6;
    if version != RTP_VERSION {
        return Err(RtpParseError::UnsupportedVersion(version));
    }
    let padding = (fixed[0] & 0b0010_0000) != 0;
    let extension = (fixed[0] & 0b0001_0000) != 0;
    let csrc_count = fixed[0] & 0x0f;
    if csrc_count as usize > MAX_CSRC_COUNT {
        return Err(RtpParseError::InvalidCsrcLength);
    }
    let marker = (fixed[1] & 0b1000_0000) != 0;
    let payload_type = fixed[1] & 0x7f;
    let sequence_number = u16::from_be_bytes([fixed[2], fixed[3]]);
    let timestamp = u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]);
    let ssrc = u32::from_be_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]);

    let mut header = RtpHeader {
        version,
        padding,
        extension,
        csrc_count,
        marker,
        payload_type,
        sequence_number,
        timestamp,
        ssrc,
        csrcs: [0; MAX_CSRC_COUNT],
    };

    let mut cursor = RTP_HEADER_MIN_LEN;
    let csrc_bytes = csrc_count as usize * 4;
    if total_len < cursor + csrc_bytes {
        return Err(RtpParseError::InvalidCsrcLength);
    }
    if csrc_bytes > 0 {
        let mut word = [0u8; 4];
        for idx in 0..(csrc_count as usize) {
            if !read_into(view, cursor + idx * 4, &mut word) {
                return Err(RtpParseError::InvalidCsrcLength);
            }
            header.csrcs[idx] = u32::from_be_bytes(word);
        }
    }
    cursor += csrc_bytes;

    let mut extension_meta: Option<RtpHeaderExtension> = None;
    if extension {
        if total_len < cursor + 4 {
            return Err(RtpParseError::InvalidExtension);
        }
        let mut ext_header = [0u8; 4];
        if !read_into(view, cursor, &mut ext_header) {
            return Err(RtpParseError::InvalidExtension);
        }
        let profile = u16::from_be_bytes([ext_header[0], ext_header[1]]);
        let length_words = u16::from_be_bytes([ext_header[2], ext_header[3]]) as usize;
        let extension_len = length_words
            .checked_mul(4)
            .ok_or(RtpParseError::InvalidExtension)?;
        cursor += 4;
        if total_len < cursor + extension_len {
            return Err(RtpParseError::InvalidExtension);
        }
        extension_meta = Some(RtpHeaderExtension {
            profile,
            span: ByteSpan::new(cursor, extension_len),
        });
        cursor += extension_len;
    }

    if cursor > total_len {
        return Err(RtpParseError::HeaderTooShort);
    }

    let mut padding_len = 0u8;
    if padding {
        let last_byte_offset = total_len
            .checked_sub(1)
            .ok_or(RtpParseError::InvalidPadding)?;
        let pad_value = read_u8(view, last_byte_offset).ok_or(RtpParseError::InvalidPadding)?;
        if pad_value == 0 {
            return Err(RtpParseError::InvalidPadding);
        }
        let pad_len = pad_value as usize;
        if pad_len == 0 || pad_len > total_len.saturating_sub(cursor) {
            return Err(RtpParseError::InvalidPadding);
        }
        padding_len = pad_value;
    }

    let payload_available = total_len
        .checked_sub(cursor)
        .ok_or(RtpParseError::InvalidPadding)?;
    let payload_len = payload_available
        .checked_sub(padding_len as usize)
        .ok_or(RtpParseError::InvalidPadding)?;

    Ok(RtpPacket {
        header,
        view,
        payload: ByteSpan::new(cursor, payload_len),
        extension: extension_meta,
        padding_len,
    })
}

/// 序列号回绕安全比较：判断 `a` 是否在回绕语义下「早于」 `b`。
///
/// - **算法来源**：RFC 3550 附录 A，基于半区差分判断顺序关系。
/// - **返回值**：若 `a` 应被视为较旧序列号，则返回 `true`；相等时返回 `false`。
#[must_use]
pub fn seq_less(a: u16, b: u16) -> bool {
    let diff = b.wrapping_sub(a);
    diff != 0 && diff < 0x8000
}

/// 将指定区间映射为零拷贝分片。
fn slice_chunks<'a>(view: &'a dyn BufView, span: ByteSpan) -> Chunks<'a> {
    if span.is_empty() {
        return Chunks::empty();
    }
    let end = match span.end() {
        Some(end) => end,
        None => return Chunks::empty(),
    };
    let mut collected: Vec<&'a [u8]> = Vec::new();
    let mut offset = 0usize;
    for chunk in view.as_chunks() {
        let chunk_start = offset;
        let chunk_end = offset + chunk.len();
        if chunk_end <= span.offset {
            offset = chunk_end;
            continue;
        }
        if chunk_start >= end {
            break;
        }
        let start_in_chunk = span.offset.saturating_sub(chunk_start);
        let end_in_chunk = if end < chunk_end {
            end - chunk_start
        } else {
            chunk.len()
        };
        if end_in_chunk > start_in_chunk {
            collected.push(&chunk[start_in_chunk..end_in_chunk]);
        }
        offset = chunk_end;
    }
    Chunks::from_vec(collected)
}

fn read_into(view: &dyn BufView, offset: usize, buf: &mut [u8]) -> bool {
    let end = match offset.checked_add(buf.len()) {
        Some(end) => end,
        None => return false,
    };
    let mut written = 0usize;
    let mut cursor = 0usize;
    for chunk in view.as_chunks() {
        let chunk_start = cursor;
        let chunk_end = cursor + chunk.len();
        if chunk_end <= offset {
            cursor = chunk_end;
            continue;
        }
        if chunk_start >= end {
            break;
        }
        let start_in_chunk = offset.saturating_sub(chunk_start);
        let end_in_chunk = if end < chunk_end {
            end - chunk_start
        } else {
            chunk.len()
        };
        if end_in_chunk <= start_in_chunk {
            cursor = chunk_end;
            continue;
        }
        let slice = &chunk[start_in_chunk..end_in_chunk];
        let len = slice.len();
        buf[written..written + len].copy_from_slice(slice);
        written += len;
        if written == buf.len() {
            return true;
        }
        cursor = chunk_end;
    }
    written == buf.len()
}

fn read_u8(view: &dyn BufView, offset: usize) -> Option<u8> {
    let mut buf = [0u8; 1];
    if read_into(view, offset, &mut buf) {
        Some(buf[0])
    } else {
        None
    }
}

impl<'a> RtpPacket<'a> {
    /// 返回解析后的 header 引用。
    #[must_use]
    pub fn header(&self) -> &RtpHeader {
        &self.header
    }

    /// 返回 payload 的零拷贝窗口。
    #[must_use]
    pub fn payload(&self) -> RtpByteSection<'a> {
        RtpByteSection::new(self.view, self.payload)
    }

    /// 返回可选的扩展视图。
    #[must_use]
    pub fn extension(&self) -> Option<RtpHeaderExtensionView<'a>> {
        self.extension.map(|ext| RtpHeaderExtensionView {
            profile: ext.profile,
            data: RtpByteSection::new(self.view, ext.span),
        })
    }

    /// 返回尾部填充字节长度（若无 padding 则为 0）。
    #[must_use]
    pub fn padding_len(&self) -> u8 {
        self.padding_len
    }
}

impl fmt::Debug for RtpPacket<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtpPacket")
            .field("header", &self.header)
            .field("payload_len", &self.payload.len())
            .field(
                "extension",
                &self
                    .extension
                    .as_ref()
                    .map(|ext| (ext.profile, ext.span.offset, ext.span.len())),
            )
            .field("padding_len", &self.padding_len)
            .finish()
    }
}

/// RTP 报文基础生成器，负责在调用方提供 header/扩展/payload 信息后拼装字节流。
#[derive(Clone)]
pub struct RtpPacketBuilder<'a> {
    header: RtpHeader,
    payload: Option<&'a dyn BufView>,
    extension: Option<RtpPacketBuilderExtension<'a>>,
    padding_len: u8,
}

#[derive(Debug, Clone)]
struct RtpPacketBuilderExtension<'a> {
    profile: u16,
    data: &'a [u8],
}

impl<'a> RtpPacketBuilder<'a> {
    /// 创建新的生成器实例。
    ///
    /// - **前置条件**：调用方需保证传入的 header 字段已经按照业务需求设定。
    #[must_use]
    pub fn new(header: RtpHeader) -> Self {
        Self {
            header,
            payload: None,
            extension: None,
            padding_len: 0,
        }
    }

    /// 指定零拷贝 payload 来源。
    #[must_use]
    pub fn payload_view(mut self, payload: &'a dyn BufView) -> Self {
        self.payload = Some(payload);
        self
    }

    /// 配置 header 扩展数据。
    pub fn extension_bytes(mut self, profile: u16, data: &'a [u8]) -> Result<Self, RtpEncodeError> {
        if data.len() % 4 != 0 {
            return Err(RtpEncodeError::HeaderMismatch(
                "扩展数据长度必须是 32-bit word 的整数倍",
            ));
        }
        self.extension = Some(RtpPacketBuilderExtension { profile, data });
        Ok(self)
    }

    /// 指定 padding 长度（若为 0 则表示无填充）。
    #[must_use]
    pub fn padding(mut self, padding_len: u8) -> Self {
        self.padding_len = padding_len;
        self
    }

    /// 将 RTP 报文编码到输出缓冲中，并返回实际写入字节数。
    ///
    /// - **错误条件**：当 header 字段不符合契约或输出缓冲过小时返回 `RtpEncodeError`。
    pub fn encode_into(self, dst: &mut [u8]) -> Result<usize, RtpEncodeError> {
        if self.header.version != RTP_VERSION {
            return Err(RtpEncodeError::InvalidField("version"));
        }
        if self.header.payload_type > 0x7f {
            return Err(RtpEncodeError::InvalidField("payload_type"));
        }
        if self.header.csrc_count as usize > MAX_CSRC_COUNT {
            return Err(RtpEncodeError::InvalidField("csrc_count"));
        }

        let payload_len = self.payload.map_or(0, |payload| payload.len());
        let csrc_bytes = self.header.csrc_count as usize * 4;
        let ext_bytes = if self.header.extension { 4 } else { 0 };
        let extension = match (self.header.extension, self.extension) {
            (true, Some(ext)) => ext,
            (true, None) => {
                return Err(RtpEncodeError::HeaderMismatch(
                    "header.extension=1 但未提供扩展数据",
                ));
            }
            (false, Some(_)) => {
                return Err(RtpEncodeError::HeaderMismatch(
                    "header.extension=0 但调用方提供了扩展数据",
                ));
            }
            (false, None) => RtpPacketBuilderExtension {
                profile: 0,
                data: &[],
            },
        };
        let extension_len = extension.data.len();
        if extension_len > 0 && extension_len % 4 != 0 {
            return Err(RtpEncodeError::HeaderMismatch(
                "扩展数据长度必须按 32-bit word 对齐",
            ));
        }

        let padding_len = self.padding_len as usize;
        if self.header.padding {
            if padding_len == 0 {
                return Err(RtpEncodeError::HeaderMismatch(
                    "header.padding=1 但未指定 padding 长度",
                ));
            }
        } else if padding_len > 0 {
            return Err(RtpEncodeError::HeaderMismatch(
                "header.padding=0 但仍然指定了 padding",
            ));
        }

        let required =
            RTP_HEADER_MIN_LEN + csrc_bytes + ext_bytes + extension_len + payload_len + padding_len;
        if dst.len() < required {
            return Err(RtpEncodeError::BufferTooSmall);
        }

        // 写入固定头部。
        dst[0] = (self.header.version << 6)
            | ((self.header.padding as u8) << 5)
            | ((self.header.extension as u8) << 4)
            | (self.header.csrc_count & 0x0f);
        dst[1] = ((self.header.marker as u8) << 7) | (self.header.payload_type & 0x7f);
        dst[2..4].copy_from_slice(&self.header.sequence_number.to_be_bytes());
        dst[4..8].copy_from_slice(&self.header.timestamp.to_be_bytes());
        dst[8..12].copy_from_slice(&self.header.ssrc.to_be_bytes());

        // 写入 CSRC 列表。
        let mut cursor = RTP_HEADER_MIN_LEN;
        for idx in 0..(self.header.csrc_count as usize) {
            dst[cursor..cursor + 4].copy_from_slice(&self.header.csrcs[idx].to_be_bytes());
            cursor += 4;
        }

        // 写入扩展头与数据。
        if self.header.extension {
            dst[cursor..cursor + 2].copy_from_slice(&extension.profile.to_be_bytes());
            let length_words = (extension_len / 4) as u16;
            dst[cursor + 2..cursor + 4].copy_from_slice(&length_words.to_be_bytes());
            cursor += 4;
            dst[cursor..cursor + extension_len].copy_from_slice(extension.data);
            cursor += extension_len;
        }

        // 写入 payload。
        if let Some(payload) = self.payload {
            for chunk in payload.as_chunks() {
                let end = cursor + chunk.len();
                dst[cursor..end].copy_from_slice(chunk);
                cursor = end;
            }
        }

        // 写入 padding。RFC 3550 要求所有 padding 字节取值均等于 padding 长度。
        if padding_len > 0 {
            let padding_value = self.padding_len;
            for byte in &mut dst[cursor..cursor + padding_len] {
                *byte = padding_value;
            }
            cursor += padding_len;
        }

        Ok(cursor)
    }
}

/// RTP 编解码占位结构，明确媒体数据通道的实现入口。
///
/// ### 设计意图（Why）
/// - 维持与既有调用方的类型契约，确保在扩展真正的 RTP 编解码逻辑前，依赖链保持可编译状态。
/// - 提供统一的构造接口，便于示例与文档在尚未加载完整功能时演示类型使用方法。
///
/// ### 契约说明（What）
/// - 当前不存储任何字段，仅作为占位符。
/// - 与本文件内新增的解析/生成能力互不依赖，后续可按需扩展真实逻辑。
///
/// ### 实现策略（How）
/// - 使用零尺寸类型配合 `Default`/`Copy`，保持构造与复制成本为零。
///
/// ### 风险提示（Trade-offs）
/// - 当未来替换为真实实现时，需要评估线程安全与状态同步问题。
#[derive(Debug, Default, Clone, Copy)]
pub struct RtpCodecScaffold;

impl RtpCodecScaffold {
    /// 构造 RTP 编解码占位实例。
    ///
    /// ### 设计动机（Why）
    /// - 为 `spark-impl-tck` 提供稳定的类型依赖，便于提前编写测试脚本。
    /// - 对外暴露统一的构造入口，方便未来扩展参数。
    ///
    /// ### 契约定义（What）
    /// - **输入**：无。
    /// - **输出**：`RtpCodecScaffold` 的零尺寸实例。
    /// - **前置条件**：调用方只需链接本 crate 即可，无额外资源需求。
    /// - **后置条件**：获得一个可复制的占位类型，可用于泛型约束或测试桩。
    ///
    /// ### 实现说明（How）
    /// - `const fn` 允许在编译期常量上下文中使用（例如默认配置）。
    ///
    /// ### 风险提示（Trade-offs）
    /// - 随着功能完善可能需要返回 `Result` 表达错误；届时需同步更新所有调用方。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// RTP 解析错误的分类枚举，明确调用方可预期的失败场景。
///
/// - **Why**：在 TCK 与业务逻辑中区分「输入不足」「字段非法」等问题，有助于定位具体的网络或实现缺陷。
/// - **How**：解析过程中一旦检测到违反 RFC 3550 的情况即返回对应枚举值，避免继续消费原始缓冲。
/// - **Contract**：所有错误均为可复制的枚举，便于在 `no_std` 环境中使用（无需依赖 `std::error::Error`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtpParseError {
    /// 输入缓冲长度不足以覆盖固定 12 字节头部。
    HeaderTooShort,
    /// RTP 版本号不符合期望（仅支持版本 2）。
    UnsupportedVersion(u8),
    /// CSRC 列表长度超过 15 或输入字节不足以容纳完整列表。
    InvalidCsrcLength,
    /// 扩展头标记与实际字节数不一致，或扩展长度不是 32-bit words。
    InvalidExtension,
    /// Padding 标记设置但尾部字节不足以表示指定的填充长度。
    InvalidPadding,
}

impl fmt::Display for RtpParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooShort => f.write_str("RTP 头部不足 12 字节"),
            Self::UnsupportedVersion(v) => {
                write!(f, "不支持的 RTP 版本号：{} (仅支持 2)", v)
            }
            Self::InvalidCsrcLength => f.write_str("CSRC 列表长度非法或字节不足"),
            Self::InvalidExtension => f.write_str("扩展头字段与实际数据不一致"),
            Self::InvalidPadding => f.write_str("Padding 标记非法或填充长度不足"),
        }
    }
}

/// RTP 生成过程的错误枚举，指示构造报文时的契约违背情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtpEncodeError {
    /// 输出缓冲不足以容纳完整的 RTP 报文。
    BufferTooSmall,
    /// Header 中的字段与扩展/填充配置不匹配，例如 `extension` 位为假但提供了扩展数据。
    HeaderMismatch(&'static str),
    /// Payload Type 超出 7 bit 范围或其它字段违反基础格式。
    InvalidField(&'static str),
}

impl fmt::Display for RtpEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferTooSmall => f.write_str("输出缓冲区不足"),
            Self::HeaderMismatch(msg) => write!(f, "Header 与生成配置不一致：{}", msg),
            Self::InvalidField(field) => write!(f, "字段取值非法：{}", field),
        }
    }
}

/// 表示 payload 或扩展在原始缓冲中的逻辑位置。
///
/// - **Why**：保持零拷贝要求，避免在解析时复制大量 payload。
/// - **How**：记录起始偏移与长度，在需要暴露 `Chunks` 时动态切片原始 `BufView`。
/// - **Contract**：偏移与长度组合必须在源缓冲范围内，`len` 可为 0 表示空区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSpan {
    offset: usize,
    len: usize,
}

impl ByteSpan {
    /// 构造新的 `ByteSpan`。
    ///
    /// - **输入**：`offset` 为起始偏移，`len` 为连续字节数。
    /// - **前置条件**：调用方需确保 `offset + len` 不发生溢出且不超过底层缓冲长度。
    /// - **后置条件**：生成的结构可用于从 `BufView` 派生零拷贝窗口。
    #[must_use]
    pub const fn new(offset: usize, len: usize) -> Self {
        Self { offset, len }
    }

    /// 返回跨度长度。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// 返回是否为空区间。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 计算结束位置（开区间），若发生溢出则返回 `None`。
    fn end(&self) -> Option<usize> {
        self.offset.checked_add(self.len)
    }
}
