//! RTCP 统计报文（SR/RR）生成工具。
//!
//! # 教案定位（Why）
//! - 发送端需要周期性构造 Sender Report (SR) 与 Receiver Report (RR) 以同步时钟并反馈网络质量。
//! - TCK 在本阶段通过 `build_sr`/`build_rr` 验证报文编码的契约，确保实现者能正确理解 RFC3550 §6.3.1 / §6.4.1。
//!
//! # 接口职责（What）
//! - `RtpClockMapper`：封装 NTP ↔ RTP 的时间映射，统一管理采样起点、时钟频率与回绕规则。
//! - `SenderStat`/`ReceiverStat`：以结构化方式描述待上报的统计数据，避免在调用处拼接裸字节。
//! - `build_sr`/`build_rr`：根据输入统计生成符合协议的 RTCP 报文并写入缓冲区。
//!
//! # 实现策略（How）
//! - 采用整数运算完成 NTP 时间戳与 RTP tick 的换算，杜绝浮点累计误差；
//! - 在写入报文前进行契约校验（报告块数量、Profile 扩展对齐、累计丢包范围等），提前捕获编码错误；
//! - 统一以 `Vec<u8>` 作为输出缓冲，方便与后续复合报文生成器对接。
//!
//! # 风险提示（Trade-offs）
//! - 当前实现依赖 `std::time::SystemTime`；在 `no_std`/`alloc` 构建下该模块会被禁用，调用方需自行提供替代实现；
//! - RTP 时间戳只进行 32-bit 回绕处理，若长时间运行需由上层逻辑维护额外的回绕上下文。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloc::vec::Vec;

/// RTCP 的固定版本号（RFC3550 §6.1）。
const RTCP_VERSION: u8 = 2;

/// RTCP Sender Report 的分组类型常量（RFC3550 §6.4.1）。
const RTCP_SR_PACKET_TYPE: u8 = 200;

/// RTCP Receiver Report 的分组类型常量（RFC3550 §6.4.2）。
const RTCP_RR_PACKET_TYPE: u8 = 201;

/// NTP 时间戳与 Unix 纪元之间的秒差（1970-01-01 与 1900-01-01 的间隔）。
const NTP_UNIX_EPOCH_DELTA_SECS: u64 = 2_208_988_800;

/// 当累计丢包超出 24-bit 表示范围时返回的错误。
const CUMULATIVE_LOST_MIN: i32 = -0x80_0000;
const CUMULATIVE_LOST_MAX: i32 = 0x7F_FFFF;

/// 构造 RTCP 统计报文过程中可能出现的错误。
///
/// ### Why
/// - 发送/接收报告的编码存在多处协议约束，直接 `panic` 会给上层带来不可控的崩溃风险。
/// - 将错误枚举化后，调用方可以在调试阶段快速定位违反协议的根因。
///
/// ### What
/// - 每个变体对应一类契约违规：报告块数量超限、扩展字段未按 32-bit 对齐、时间回退等。
/// - 错误在 `build_sr`/`build_rr` 中返回，调用方可据此选择回滚或丢弃统计。
///
/// ### How
/// - 仅使用 `#[derive(Debug, Clone, PartialEq, Eq)]`，便于单元测试直接断言；
/// - 未实现 `std::error::Error`，因为该模块在 `no_std` 环境下默认禁用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// 报告块数量超出 5 bit 所能表示的范围（RFC3550 上限 31）。
    TooManyReports {
        /// 实际提供的报告块数量。
        count: usize,
    },
    /// Profile 扩展长度不是 32-bit 对齐，违反 §6.4.1/§6.4.2 的对齐约束。
    MisalignedProfileExtension {
        /// Profile 扩展字段的原始字节长度。
        len: usize,
    },
    /// `SystemTime` 早于 1900-01-01，无法映射为合法的 NTP 时间戳。
    SystemTimeBeforeNtpEpoch,
    /// 采样时间早于 `RtpClockMapper` 的参考起点，意味着时钟倒退。
    ClockWentBackwards,
    /// 接收报告中的 `cumulative_lost` 超出 24-bit 有符号整数的表示范围。
    CumulativeLostOutOfRange {
        /// 调用方传入的累计丢包值。
        value: i32,
    },
}

/// RTP 与 NTP 时间映射器。
///
/// ### 意图说明（Why）
/// - SR 需要同时携带 64-bit NTP 时间戳与 RTP 时钟刻度，二者必须严格对应同一采样时刻。
/// - 通过集中管理起始时间与时钟频率，可以确保所有调用者共享一致的换算规则。
///
/// ### 契约（What）
/// - `origin_time`：对应 `origin_rtp` 的绝对时间，用于之后的差值计算；
/// - `origin_rtp`：RTP 时间戳零点（或任意参考点），允许在 32-bit 范围内回绕；
/// - `clock_rate`：RTP 时钟频率（单位：ticks/秒），必须为正数；
/// - `snapshot`：将任意 `SystemTime` 映射为 `(ntp_timestamp, rtp_timestamp)`。
///
/// ### 实现细节（How）
/// - 差值计算基于整数运算：`ticks = secs * clock_rate + nanos * clock_rate / 1e9`；
/// - NTP 时间戳通过向 Unix 时间加上固定偏移（2208988800 秒）获得；
/// - `rtp_timestamp` 使用 `wrapping_add` 处理 32-bit 回绕。
///
/// ### 风险提示（Trade-offs）
/// - 假设调用方提供的 `SystemTime` 精度不低于纳秒；若底层平台精度更低可能导致 RTP tick 取整误差；
/// - 当 `clock_rate` 极大时（> 4GHz）`u128` 计算仍然安全，但输出转换为 `u32` 时会发生截断，符合 RTP 模 2^32 的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpClockMapper {
    origin_time: SystemTime,
    origin_rtp: u32,
    clock_rate: u32,
}

impl RtpClockMapper {
    /// 使用给定的起点与时钟频率构造映射器。
    ///
    /// ### 输入说明
    /// - `origin_time`：定义 RTP 时间戳 `origin_rtp` 对应的绝对时间。
    /// - `origin_rtp`：选择的参考 RTP 时间戳，可从媒体初始化阶段获取。
    /// - `clock_rate`：媒体时钟频率；若为 0 表示缺失配置，将在生成时直接返回错误。
    ///
    /// ### 行为描述
    /// - 构造函数不执行额外校验，仅保存参数；
    /// - `clock_rate = 0` 不会立即报错，但在调用 `snapshot` 时会导致 RTP 时间差始终为 0。
    #[must_use]
    pub const fn new(origin_time: SystemTime, origin_rtp: u32, clock_rate: u32) -> Self {
        Self {
            origin_time,
            origin_rtp,
            clock_rate,
        }
    }

    /// 返回内部记录的 RTP 时钟频率。
    #[must_use]
    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }

    /// 将指定的采样时间映射为 NTP 与 RTP 时间戳快照。
    ///
    /// ### 合同说明
    /// - **输入**：任意 `SystemTime`，通常来自媒体帧的采样时间；
    /// - **输出**：`Ok((ntp_timestamp, rtp_timestamp))`，两者均已经过 mod 处理；
    /// - **前置条件**：`capture_time >= origin_time`；否则返回 [`BuildError::ClockWentBackwards`]；
    /// - **后置条件**：返回的 NTP/RTP 时间戳指向相同的物理时刻，可直接写入 SR。
    pub fn snapshot(&self, capture_time: SystemTime) -> Result<(u64, u32), BuildError> {
        let ntp = system_time_to_ntp(capture_time)?;
        let delta = capture_time
            .duration_since(self.origin_time)
            .map_err(|_| BuildError::ClockWentBackwards)?;
        let ticks = duration_to_rtp_ticks(delta, self.clock_rate);
        let rtp_timestamp = self.origin_rtp.wrapping_add(ticks as u32);
        Ok((ntp, rtp_timestamp))
    }
}

/// 发送端统计输入，描述 SR 报文需要携带的所有字段。
///
/// ### Why
/// - 将 SR 所需的元数据聚合成结构体，可在调用处显式声明每个字段的含义，避免魔法常量。
///
/// ### What
/// - `sender_ssrc`：发送端自身的同步源标识；
/// - `capture_time`：用于计算 NTP/RTP 时间戳的采样时间；
/// - `sender_packet_count`/`sender_octet_count`：RFC3550 §6.4.1 的统计字段；
/// - `reports`：SR 中携带的接收报告块集合；
/// - `profile_extensions`：可选的 Profile-Specific 扩展，需要由 32-bit 边界对齐。
///
/// ### 前置/后置条件
/// - **前置**：`reports.len() <= 31`、`profile_extensions.len()` 必须是 4 的倍数；
/// - **后置**：成功生成的 SR 将把所有字段按照网络字节序写入输出缓冲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderStat<'a> {
    /// 发送端 SSRC。
    pub sender_ssrc: u32,
    /// 采样时间，使用 `RtpClockMapper` 映射为 NTP/RTP 时间戳。
    pub capture_time: SystemTime,
    /// 自上次报告以来累计发送的 RTP 包个数。
    pub sender_packet_count: u32,
    /// 自上次报告以来累计发送的负载字节数。
    pub sender_octet_count: u32,
    /// 接收报告块集合。
    pub reports: &'a [ReceptionStat],
    /// Profile-Specific 扩展字段（已对齐到 32-bit）。
    pub profile_extensions: &'a [u8],
}

/// 接收端统计输入，描述 RR 报文所需的字段。
///
/// ### Why
/// - RR 不含发送统计，但仍需要聚合报告块与扩展字段，结构体保持与 SR 对称便于复用测试。
///
/// ### 契约（What）
/// - `reporter_ssrc`：上报该 RR 的节点；
/// - `reports`：针对不同源的接收报告块；
/// - `profile_extensions`：Profile-Specific 扩展，同样要求 32-bit 对齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverStat<'a> {
    /// 报告方 SSRC。
    pub reporter_ssrc: u32,
    /// 接收报告块集合。
    pub reports: &'a [ReceptionStat],
    /// Profile 扩展原始字节。
    pub profile_extensions: &'a [u8],
}

/// 接收报告块的输入结构，映射为 24 字节的 `ReceptionReport`。
///
/// ### 契约说明
/// - 字段与 [`crate::packet::ReceptionReport`] 一一对应；
/// - `cumulative_lost` 采用 24-bit 有符号整数语义，范围 `[-8388608, 8388607]`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceptionStat {
    /// 被观测源的同步源标识。
    pub source_ssrc: u32,
    /// 上一统计间隔的瞬时丢包率。
    pub fraction_lost: u8,
    /// 累计丢包数量（24-bit 有符号整数范围）。
    pub cumulative_lost: i32,
    /// 接收方观测到的最高扩展序列号。
    pub extended_highest_sequence: u32,
    /// 接收端估计的到达抖动。
    pub interarrival_jitter: u32,
    /// 最近一次接收到的 SR 时间戳（LSR）。
    pub last_sr_timestamp: u32,
    /// 从 LSR 到当前 RR 的间隔，单位 1/65536 秒。
    pub delay_since_last_sr: u32,
}

/// 构造 Sender Report 报文并写入输出缓冲。
///
/// ### 使用说明（How）
/// 1. 调用方准备 `RtpClockMapper` 与 `SenderStat`；
/// 2. 该函数会计算 NTP/RTP 时间戳、写入 header、固定字段、报告块以及扩展字段；
/// 3. 若任一步骤违反契约则返回 [`BuildError`]。
///
/// ### 前置条件（What）
/// - `sender_stat.reports.len() <= 31`；
/// - `sender_stat.profile_extensions.len()` 为 4 的倍数；
/// - `sender_stat.capture_time` 不早于 `clock.origin_time` 且不早于 1900-01-01。
///
/// ### 后置条件
/// - `out` 追加完整的 SR 报文，长度等于 `(length + 1) * 4`；
/// - 输出缓冲未进行清空操作，允许调用方复用同一个 `Vec` 生成多个报文。
pub fn build_sr(
    clock: &RtpClockMapper,
    sender_stat: &SenderStat<'_>,
    out: &mut Vec<u8>,
) -> Result<(), BuildError> {
    validate_report_constraints(sender_stat.reports, sender_stat.profile_extensions)?;
    let (ntp_timestamp, rtp_timestamp) = clock.snapshot(sender_stat.capture_time)?;

    let report_count = sender_stat.reports.len();
    let payload_len = 4  // sender_ssrc
        + 20 // sender info
        + report_count * 24
        + sender_stat.profile_extensions.len();
    let total_len = 4 + payload_len;
    let length_field = bytes_to_rtcp_length(total_len);

    write_header(out, report_count as u8, RTCP_SR_PACKET_TYPE, length_field);
    out.extend_from_slice(&sender_stat.sender_ssrc.to_be_bytes());
    out.extend_from_slice(&ntp_timestamp.to_be_bytes());
    out.extend_from_slice(&rtp_timestamp.to_be_bytes());
    out.extend_from_slice(&sender_stat.sender_packet_count.to_be_bytes());
    out.extend_from_slice(&sender_stat.sender_octet_count.to_be_bytes());
    write_reception_reports(out, sender_stat.reports)?;
    out.extend_from_slice(sender_stat.profile_extensions);
    Ok(())
}

/// 构造 Receiver Report 报文并写入输出缓冲。
///
/// ### 契约摘要
/// - 校验报告块数量与扩展字段对齐；
/// - 无需进行 NTP/RTP 映射；
/// - 输出缓冲追加完整 RR 报文。
pub fn build_rr(recv_stat: &ReceiverStat<'_>, out: &mut Vec<u8>) -> Result<(), BuildError> {
    validate_report_constraints(recv_stat.reports, recv_stat.profile_extensions)?;
    let report_count = recv_stat.reports.len();
    let payload_len = 4 // reporter_ssrc
        + report_count * 24
        + recv_stat.profile_extensions.len();
    let total_len = 4 + payload_len;
    let length_field = bytes_to_rtcp_length(total_len);

    write_header(out, report_count as u8, RTCP_RR_PACKET_TYPE, length_field);
    out.extend_from_slice(&recv_stat.reporter_ssrc.to_be_bytes());
    write_reception_reports(out, recv_stat.reports)?;
    out.extend_from_slice(recv_stat.profile_extensions);
    Ok(())
}

/// 校验报告块数量与 Profile 扩展长度是否满足协议约束。
fn validate_report_constraints(
    reports: &[ReceptionStat],
    profile_extensions: &[u8],
) -> Result<(), BuildError> {
    if reports.len() > 31 {
        return Err(BuildError::TooManyReports {
            count: reports.len(),
        });
    }
    if profile_extensions.len() % 4 != 0 {
        return Err(BuildError::MisalignedProfileExtension {
            len: profile_extensions.len(),
        });
    }
    Ok(())
}

/// 将字节长度转换为 RTCP 头部的 `length` 字段值。
fn bytes_to_rtcp_length(total_len: usize) -> u16 {
    debug_assert_eq!(total_len % 4, 0, "RTCP 报文长度必须按 32-bit 对齐");
    ((total_len / 4) - 1) as u16
}

/// 写入 RTCP 报文头部。
fn write_header(out: &mut Vec<u8>, report_count: u8, packet_type: u8, length_field: u16) {
    let v_p_count = (RTCP_VERSION << 6) | (report_count & 0x1F);
    out.extend_from_slice(&[
        v_p_count,
        packet_type,
        (length_field >> 8) as u8,
        (length_field & 0xFF) as u8,
    ]);
}

/// 写入接收报告块列表。
fn write_reception_reports(out: &mut Vec<u8>, reports: &[ReceptionStat]) -> Result<(), BuildError> {
    for report in reports {
        out.extend_from_slice(&report.source_ssrc.to_be_bytes());
        out.push(report.fraction_lost);
        let cumulative_lost = encode_cumulative_lost(report.cumulative_lost)?;
        out.extend_from_slice(&cumulative_lost);
        out.extend_from_slice(&report.extended_highest_sequence.to_be_bytes());
        out.extend_from_slice(&report.interarrival_jitter.to_be_bytes());
        out.extend_from_slice(&report.last_sr_timestamp.to_be_bytes());
        out.extend_from_slice(&report.delay_since_last_sr.to_be_bytes());
    }
    Ok(())
}

/// 将 24-bit 的 `cumulative_lost` 编码为网络字节序字节数组。
fn encode_cumulative_lost(value: i32) -> Result<[u8; 3], BuildError> {
    if !(CUMULATIVE_LOST_MIN..=CUMULATIVE_LOST_MAX).contains(&value) {
        return Err(BuildError::CumulativeLostOutOfRange { value });
    }
    let encoded = (value as i64) & 0x00FF_FFFF;
    Ok([
        ((encoded >> 16) & 0xFF) as u8,
        ((encoded >> 8) & 0xFF) as u8,
        (encoded & 0xFF) as u8,
    ])
}

/// 将 `SystemTime` 转换为 64-bit NTP 时间戳。
fn system_time_to_ntp(time: SystemTime) -> Result<u64, BuildError> {
    let since_unix = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BuildError::SystemTimeBeforeNtpEpoch)?;
    let total_secs = since_unix
        .as_secs()
        .saturating_add(NTP_UNIX_EPOCH_DELTA_SECS);
    let fractional = ((since_unix.subsec_nanos() as u128) << 32) / 1_000_000_000u128;
    Ok((total_secs << 32) | (fractional as u64))
}

/// 将持续时间转换为 RTP tick 数量。
fn duration_to_rtp_ticks(duration: Duration, clock_rate: u32) -> u128 {
    if clock_rate == 0 {
        return 0;
    }
    let rate = clock_rate as u128;
    let whole = duration.as_secs() as u128 * rate;
    let frac = (duration.subsec_nanos() as u128 * rate) / 1_000_000_000u128;
    whole + frac
}
