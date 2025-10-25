//! RTCP 报告生成与统计映射辅助工具。
//!
//! # 教案定位（Why）
//! - 将“统计数据 → RTCP 报文”这一算法性逻辑集中管理，便于协议实现和测试套件复用相同的构建流程。
//! - 通过对 NTP/RTP 时间轴的显式描述，帮助读者理解 Sender Report 与 Receiver Report 之间的时间相关字段来源。
//!
//! # 模块契约（What）
//! - 暴露 `build_sr`/`build_rr` 两个构建函数，分别生成带统计字段的 Sender Report、Receiver Report 报文。
//! - 定义 `NtpTime`、`SenderStatistics`、`ReceptionStatistics` 等结构，明确上层调用方需要提供的统计指标格式。
//! - 约束调用方在输入时满足 RFC3550 对字段范围、对齐的要求；若数据超界，本模块会按规范执行饱和或默认降级处理。
//!
//! # 使用指引（How）
//! - 在采集统计后，构造 `SenderStatistics`/`ReceiverStatistics` 并调用对应的 `build_*` 函数，将结果推入 `RtcpPacketVec`。
//! - 若没有可用的 RTP 时钟，可提供自定义实现的 `RtpClock` 以完成 NTP→RTP 的时间映射。
//! - 对延迟、丢包等字段，本模块提供了单位换算和边界裁剪逻辑，调用方无需重复实现。
//!
//! # 风险提示（Trade-offs）
//! - 为兼顾 `no_std` 环境，模块使用 `alloc` 中的容器并避免标准库依赖；这要求调用方在 `no_std` 场景下启用 `alloc` 特性。
//! - `build_sr`/`build_rr` 会复制输入的切片数据，若报告量较大需留意堆分配开销；可在上层复用缓冲或启用对象池优化。

use alloc::vec::Vec;
use core::{cmp, time::Duration};

use crate::{ReceiverReport, ReceptionReport, RtcpPacket, RtcpPacketVec, SenderInfo, SenderReport};

/// RTP 媒体时钟抽象，用于在生成 SR 时完成 NTP 时间到 RTP 时间戳的映射。
///
/// ## 教案说明（Why）
/// - RFC3550 §6.3.1 要求 SR 同时携带 NTP 与 RTP 时间戳，用于音视频同步；实现者往往已经掌握一套“墙钟 → RTP”转换逻辑。
/// - 通过 trait 约定，将该转换逻辑注入构建流程，避免在 `build_sr` 内部硬编码采样率或时间基。
///
/// ## 契约约束（What）
/// - `to_rtp_timestamp` 输入为 NTP 时间（单位同 RFC3550，64-bit 秒+分数）；输出应为与媒体时钟同源的 RTP 时间戳。
/// - 调用前置条件：调用者需保证提供的 `NtpTime` 与当前媒体时钟处于同一时间轴；否则 RTCP 同步字段将出现偏差。
/// - 后置条件：返回的 `u32` 将直接写入 SR 报文，范围超界将按 `u32` 自然截断，调用方需自行保证合法性。
///
/// ## 风险提示（Trade-offs）
/// - 若媒体时钟支持跳变（例如重新同步），请在实现中自行处理 wrap-around，以保持 RTP 时间戳单调递增。
pub trait RtpClock {
    /// 将 NTP 表示的时间映射为 RTP 时间戳。
    fn to_rtp_timestamp(&self, ntp: &NtpTime) -> u32;
}

/// NTP 时间戳（64-bit，32 bit 秒 + 32 bit 分数）的小型封装。
///
/// ## 教案说明（Why）
/// - RTCP 在 SR 与 RR 中多次引用 NTP 时间字段，该结构帮助调用方以类型安全的方式传递值，同时隐藏位运算细节。
/// - 封装 `as_u64`/`lsr` 等转换方法，便于生成 SenderInfo 与接收报告块。
///
/// ## 契约定义（What）
/// - `seconds` 与 `fraction` 均为 32-bit 无符号整数，遵循 RFC3550 的布局约定。
/// - 可通过 `from_parts` 构造，也可从 `Duration` 等高层时间类型换算后传入。
/// - 前置条件：`seconds`、`fraction` 应来自同一时钟基准；若跨时钟混用，SR/RR 中的同步信息将失效。
/// - 后置条件：`as_u64` 返回的 64-bit 值可直接写入网络序字段；`lsr` 提供 compact 表示（低 32-bit）。
///
/// ## 风险提示（Trade-offs）
/// - 结构不主动校验 `fraction` 是否小于 `2^32`（由类型保证），调用方需自行负责单位换算的正确性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NtpTime {
    seconds: u32,
    fraction: u32,
}

impl NtpTime {
    /// 依据给定的整数秒与小数部分构造 NTP 时间戳。
    pub const fn from_parts(seconds: u32, fraction: u32) -> Self {
        Self { seconds, fraction }
    }

    /// 返回整数秒部分。
    pub const fn seconds(&self) -> u32 {
        self.seconds
    }

    /// 返回小数部分。
    pub const fn fraction(&self) -> u32 {
        self.fraction
    }

    /// 以 64-bit 表示完整的 NTP 时间戳。
    pub const fn as_u64(&self) -> u64 {
        ((self.seconds as u64) << 32) | self.fraction as u64
    }

    /// 生成 RFC3550 中定义的 `LSR`（Last SR，32-bit Compact NTP）。
    pub const fn lsr(&self) -> u32 {
        ((self.seconds & 0xFFFF) << 16) | (self.fraction >> 16)
    }
}

/// 单个接收报告块的原始统计数据。
///
/// ## 意图阐述（Why）
/// - 提供 SR/RR 中 `reception report` 所需字段的载体，让调用者可以直接填入测量值。
/// - 聚合延迟、丢包、最高序列等指标，避免构建函数对上层逻辑有过多耦合。
///
/// ## 契约描述（What）
/// - `source_ssrc`：被报告方 SSRC；`fraction_lost`：8-bit 瞬时丢包率（0~255）。
/// - `cumulative_lost` 必须位于 `[-8_388_608, 8_388_607]` 范围；超界将被截断以遵循 24-bit 有符号整数约束。
/// - `last_sr`/`delay_since_last_sr`：若尚未接收到对端 SR，可填 `None`，构建器会在报文中写入 0。
/// - 前置条件：`delay_since_last_sr` 采用 `Duration`，其值不应超过可编码的 36 小时（`u32::MAX / 65536` 秒）；若超出将饱和至最大值。
/// - 后置条件：构建出的 `ReceptionReport` 会完全复制这些字段，并完成单位换算。
///
/// ## 风险提示（Trade-offs）
/// - 若上游统计窗口产生的 `cumulative_lost` 超出 24-bit 范围，本结构会饱和裁剪，可能导致信息损失；必要时可在上层拆分多份报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceptionStatistics {
    /// 被观测的源 SSRC，用于对齐报告块与实际媒体流。
    pub source_ssrc: u32,
    /// 最近一个测量窗口的丢包比例，范围 0~255（即 0~100%）。
    pub fraction_lost: u8,
    /// 测量起点以来累计丢失的 RTP 包数量，会在构建时裁剪到 24-bit 有符号区间。
    pub cumulative_lost: i32,
    /// 接收方观测到的最高扩展序列号，用于估算发包进度。
    pub extended_highest_sequence: u32,
    /// 接收链路估算的抖动值，单位为 RTP 时间戳刻度。
    pub interarrival_jitter: u32,
    /// 最近一次接收到的 SR 的 NTP 时间戳；缺失时填 `None`。
    pub last_sr: Option<NtpTime>,
    /// 自上次 SR 至本次报告的等待时间，单位为真实时间；缺失时填 `None`。
    pub delay_since_last_sr: Option<Duration>,
}

/// 发送端在生成 SR 时需要提供的统计快照。
///
/// ## 教案定位（Why）
/// - 将 SR 固定字段（发送者 SSRC、数据量统计）与可变部分（接收报告、Profile 扩展）组合，清晰描述调用方的职责范围。
/// - `capture_ntp` 与 `clock` 搭配，实现 SR 中 NTP 与 RTP 时间戳的同步。
///
/// ## 契约定义（What）
/// - `sender_ssrc`：当前发送者的 SSRC 标识。
/// - `capture_ntp`：生成报告时刻的 NTP 时间戳；必须与 `clock` 使用同一时间基准。
/// - `rtp_timestamp_override`：若上层已计算对应 RTP 时间戳，可在此覆盖；否则 `build_sr` 会调用 `clock` 推算。
/// - `reports`：跟随在 SR 后的接收报告块集合；可为空。
/// - `profile_extensions`：可选的 profile-specific 扩展数据，长度需满足 32-bit 对齐要求（本模块不强制检查，调用方需自律）。
/// - 前置条件：统计计数应遵循无符号 32-bit 限制（SR 规范要求）。
/// - 后置条件：`build_sr` 将把数据复制进新的 `SenderReport`，并推入外部提供的输出缓冲。
///
/// ## 风险提示（Trade-offs）
/// - Profile 扩展会产生堆分配；若频繁构建 SR，可重用缓冲或在更上层聚合复合报文以减少分配次数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderStatistics<'a> {
    /// 当前发送者的 SSRC 标识。
    pub sender_ssrc: u32,
    /// 生成报告时刻的 NTP 时间戳。
    pub capture_ntp: NtpTime,
    /// 若上层已计算出的 RTP 时间戳，可在此覆盖默认映射逻辑。
    pub rtp_timestamp_override: Option<u32>,
    /// 自上次报告以来累计发送的包数。
    pub sender_packet_count: u32,
    /// 自上次报告以来累计发送的载荷字节数。
    pub sender_octet_count: u32,
    /// 跟随在 SR 之后的接收报告块集合。
    pub reports: &'a [ReceptionStatistics],
    /// 可选的 profile-specific 扩展字节序列。
    pub profile_extensions: &'a [u8],
}

/// Receiver Report 所需的统计快照。
///
/// ## 教案说明（Why）
/// - 将报告方 SSRC、接收报告列表、profile 扩展打包，便于 `build_rr` 直接消费。
/// - 与 `SenderStatistics` 保持一致的字段组织，降低调用方心智负担。
///
/// ## 契约描述（What）
/// - `reporter_ssrc`：生成该 RR 的实体 SSRC。
/// - `reports`：接收报告块集合；可为空。
/// - `profile_extensions`：保留 profile-specific 字节序列，同样要求 32-bit 对齐（本模块不做强检）。
/// - 前置条件：上层需保证报告块指标已按 RFC3550 范围换算。
/// - 后置条件：生成的 `ReceiverReport` 会复制所有字段并推入输出缓冲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverStatistics<'a> {
    /// 报告方（接收端）的 SSRC。
    pub reporter_ssrc: u32,
    /// 接收报告块集合，可为空数组表示“无下游统计”。
    pub reports: &'a [ReceptionStatistics],
    /// Profile-specific 扩展字段原始字节序列。
    pub profile_extensions: &'a [u8],
}

/// 构建带统计字段的 Sender Report 并写入输出集合。
///
/// ## 教案讲解（Why）
/// - 将 SR 构建流程模块化，确保测试与实际实现共享同一套逻辑，降低重复代码与偏差风险。
/// - 在构建过程中完成 NTP → RTP 时间戳映射，保证跨媒体流同步的准确性。
///
/// ## 契约细节（What）
/// - `clock`：实现了 `RtpClock` 的时间映射器。
/// - `stats`：发送端统计快照。
/// - `out`：输出缓冲（通常为 `RtcpPacketVec`），函数会向其中追加一个 `RtcpPacket::SenderReport`。
/// - 前置条件：`stats.capture_ntp` 与 `clock` 使用同一时基；`stats.profile_extensions` 已按 32-bit 对齐。
/// - 后置条件：`out` 中追加的报文满足 RFC3550 §6.3.1 的字段要求；若传入的统计为空，报文仍包含基本 SenderInfo。
///
/// ## 实现步骤（How）
/// 1. 计算 RTP 时间戳：优先使用 `rtp_timestamp_override`，否则调用 `clock` 进行映射；
/// 2. 将 `SenderStatistics` 中的计数直接填入 `SenderInfo`；
/// 3. 遍历 `reports` 列表，调用内部辅助函数生成 `ReceptionReport`；
/// 4. 复制 profile 扩展后推入输出缓冲。
///
/// ## 风险提示（Trade-offs）
/// - 若 `clock` 与统计时间轴不一致，会导致 RTP 时间戳偏移，从而破坏同步；调用方在集成前需确保两者协同。
/// - 接收报告数量较多时会分配新的 `Vec`，可视需求复用缓冲以降低开销。
pub fn build_sr<C>(clock: &C, stats: &SenderStatistics<'_>, out: &mut RtcpPacketVec)
where
    C: RtpClock,
{
    let rtp_timestamp = stats
        .rtp_timestamp_override
        .unwrap_or_else(|| clock.to_rtp_timestamp(&stats.capture_ntp));

    let sender_info = SenderInfo {
        ntp_timestamp: stats.capture_ntp.as_u64(),
        rtp_timestamp,
        sender_packet_count: stats.sender_packet_count,
        sender_octet_count: stats.sender_octet_count,
    };

    let reports = stats
        .reports
        .iter()
        .map(reception_from_stats)
        .collect::<Vec<_>>();

    out.push(RtcpPacket::SenderReport(SenderReport {
        sender_ssrc: stats.sender_ssrc,
        sender_info,
        reports,
        profile_extensions: stats.profile_extensions.to_vec(),
    }));
}

/// 构建带统计字段的 Receiver Report 并写入输出集合。
///
/// ## 教案讲解（Why）
/// - Receiver Report 与 Sender Report 在接收报告块生成逻辑上高度一致，抽象函数便于复用并减少逻辑分叉。
/// - 通过统一入口，测试可以轻松验证 RR 中延迟、LSR 等字段的换算是否正确。
///
/// ## 契约细节（What）
/// - `stats`：RR 统计快照。
/// - `out`：输出缓冲，函数会向其中追加 `RtcpPacket::ReceiverReport`。
/// - 前置条件：`stats.profile_extensions` 已满足 32-bit 对齐要求。
/// - 后置条件：生成的 `ReceiverReport` 满足 RFC3550 §6.4.1 的字段约束，未提供的 LSR/DLSR 字段会按规范写零。
///
/// ## 实现步骤（How）
/// 1. 遍历 `stats.reports`，调用共享的 `reception_from_stats` 构造接收报告块；
/// 2. 复制 profile 扩展字节；
/// 3. 以 `RtcpPacket::ReceiverReport` 形式压入输出缓冲。
///
/// ## 风险提示（Trade-offs）
/// - 若输入统计缺失 `last_sr` 信息，RR 中的 LSR 与 DLSR 会是 0；确保接收链路在统计数据齐备时再调用可提高同步精度。
pub fn build_rr(stats: &ReceiverStatistics<'_>, out: &mut RtcpPacketVec) {
    let reports = stats
        .reports
        .iter()
        .map(reception_from_stats)
        .collect::<Vec<_>>();

    out.push(RtcpPacket::ReceiverReport(ReceiverReport {
        reporter_ssrc: stats.reporter_ssrc,
        reports,
        profile_extensions: stats.profile_extensions.to_vec(),
    }));
}

fn reception_from_stats(stats: &ReceptionStatistics) -> ReceptionReport {
    ReceptionReport {
        source_ssrc: stats.source_ssrc,
        fraction_lost: stats.fraction_lost,
        cumulative_lost: clamp_cumulative_lost(stats.cumulative_lost),
        extended_highest_sequence: stats.extended_highest_sequence,
        interarrival_jitter: stats.interarrival_jitter,
        last_sr_timestamp: stats.last_sr.map_or(0, |ntp| ntp.lsr()),
        delay_since_last_sr: stats
            .delay_since_last_sr
            .map_or(0, encode_delay_since_last_sr),
    }
}

const MIN_CUMULATIVE_LOST: i32 = -0x80_0000; // -8_388_608
const MAX_CUMULATIVE_LOST: i32 = 0x7F_FFFF; // 8_388_607

fn clamp_cumulative_lost(value: i32) -> i32 {
    value.clamp(MIN_CUMULATIVE_LOST, MAX_CUMULATIVE_LOST)
}

fn encode_delay_since_last_sr(delay: Duration) -> u32 {
    let secs_component = delay.as_secs();
    let nanos_component = delay.subsec_nanos();

    let coarse = secs_component.saturating_mul(65_536);
    let fine = ((nanos_component as u64) * 65_536) / 1_000_000_000;
    let total = coarse.saturating_add(fine);

    cmp::min(total, u32::MAX as u64) as u32
}
