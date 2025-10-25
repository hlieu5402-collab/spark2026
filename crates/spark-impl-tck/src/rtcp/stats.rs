//! SR/RR 生成逻辑的契约测试。
//!
//! # 教案定位（Why）
//! - 验证 `spark-codec-rtcp::stats` 模块在构造 Sender Report / Receiver Report 时对 NTP、RTP、统计字段的映射是否符合 RFC3550 要求。
//! - 为后续媒体栈实现提供回归基线，确保边界条件（延迟饱和、LSR 计算等）不会因重构而回退。
//!
//! # 测试策略（How）
//! - 构造伪造的 RTP 时钟实现，模拟 90 kHz 媒体时钟与 NTP 时间之间的映射关系。
//! - 针对 SR/RR 分别断言：SenderInfo、接收报告块、Profile 扩展等字段与输入统计一致。
//! - 覆盖异常情况，例如 `cumulative_lost` 超界及延迟溢出，确认构建函数采取饱和策略。

use core::time::Duration;

use spark_codec_rtcp::{
    build_rr, build_sr, NtpTime, ReceiverStatistics, ReceptionStatistics, RtcpPacket,
    RtcpPacketVec, RtpClock, SenderStatistics,
};

/// 90 kHz RTP 媒体时钟：RTP 时间戳 = 秒 * 90000 + (fraction >> 16)。
struct NinetyKhzClock;

impl RtpClock for NinetyKhzClock {
    fn to_rtp_timestamp(&self, ntp: &NtpTime) -> u32 {
        ntp.seconds().saturating_mul(90_000) + (ntp.fraction() >> 16)
    }
}

#[test]
fn build_sr_populates_sender_and_reports() {
    let clock = NinetyKhzClock;
    let last_sr = NtpTime::from_parts(0x0203, 0x0405_0000);
    let stats = SenderStatistics {
        sender_ssrc: 0x1122_3344,
        capture_ntp: NtpTime::from_parts(0x0123_4567, 0x89AB_CDEF),
        rtp_timestamp_override: None,
        sender_packet_count: 123_456,
        sender_octet_count: 654_321,
        reports: &[ReceptionStatistics {
            source_ssrc: 0x5566_7788,
            fraction_lost: 42,
            cumulative_lost: 12_345,
            extended_highest_sequence: 0x9ABC_DEF0,
            interarrival_jitter: 0x0102_0304,
            last_sr: Some(last_sr),
            delay_since_last_sr: Some(Duration::new(3, 500_000_000)),
        }],
        profile_extensions: &[0xDE, 0xAD, 0xBE, 0xEF],
    };

    let mut packets = RtcpPacketVec::new();
    build_sr(&clock, &stats, &mut packets);

    let packet = packets.pop().expect("SR builder must output one packet");
    let RtcpPacket::SenderReport(report) = packet else {
        panic!("expected SenderReport")
    };

    assert_eq!(report.sender_ssrc, stats.sender_ssrc);
    assert_eq!(report.profile_extensions, stats.profile_extensions);
    assert_eq!(report.sender_info.ntp_timestamp, stats.capture_ntp.as_u64());
    let expected_rtp = clock.to_rtp_timestamp(&stats.capture_ntp);
    assert_eq!(report.sender_info.rtp_timestamp, expected_rtp);
    assert_eq!(
        report.sender_info.sender_packet_count,
        stats.sender_packet_count
    );
    assert_eq!(
        report.sender_info.sender_octet_count,
        stats.sender_octet_count
    );

    assert_eq!(report.reports.len(), 1);
    let rr = &report.reports[0];
    assert_eq!(rr.source_ssrc, 0x5566_7788);
    assert_eq!(rr.fraction_lost, 42);
    assert_eq!(rr.cumulative_lost, 12_345);
    assert_eq!(rr.extended_highest_sequence, 0x9ABC_DEF0);
    assert_eq!(rr.interarrival_jitter, 0x0102_0304);
    assert_eq!(rr.last_sr_timestamp, last_sr.lsr());
    // 3.5 秒 => 3 * 65536 + 0.5 * 65536 = 229376
    assert_eq!(rr.delay_since_last_sr, 229_376);
}

#[test]
fn build_rr_respects_overrides_and_saturation() {
    let reception = ReceptionStatistics {
        source_ssrc: 0xCAFEBABE,
        fraction_lost: 255,
        cumulative_lost: 10_000_000, // 超界应被裁剪至 24-bit 上限
        extended_highest_sequence: 0x0102_0304,
        interarrival_jitter: 0x0506_0708,
        last_sr: None,
        delay_since_last_sr: Some(Duration::new(u64::from(u16::MAX) + 10, 900_000_000)),
    };

    let recv_stats = ReceiverStatistics {
        reporter_ssrc: 0xA0B1_C2D3,
        reports: &[reception],
        profile_extensions: &[1, 2, 3, 4, 5, 6, 7, 8],
    };

    let mut packets = RtcpPacketVec::new();
    build_rr(&recv_stats, &mut packets);

    match packets.pop().expect("RR builder must output one packet") {
        RtcpPacket::ReceiverReport(report) => {
            assert_eq!(report.reporter_ssrc, recv_stats.reporter_ssrc);
            assert_eq!(report.profile_extensions, recv_stats.profile_extensions);
            assert_eq!(report.reports.len(), 1);
            let block = &report.reports[0];
            assert_eq!(block.source_ssrc, reception.source_ssrc);
            assert_eq!(block.fraction_lost, 255);
            assert_eq!(block.cumulative_lost, 0x7F_FFFF);
            assert_eq!(block.last_sr_timestamp, 0);
            assert_eq!(block.delay_since_last_sr, u32::MAX);
        }
        other => panic!("unexpected packet variant: {other:?}"),
    }
}

#[test]
fn build_sr_honors_rtp_override() {
    let stats = SenderStatistics {
        sender_ssrc: 0xFEED_CAFE,
        capture_ntp: NtpTime::from_parts(10, 0),
        rtp_timestamp_override: Some(0xDEAD_BEEF),
        sender_packet_count: 1,
        sender_octet_count: 2,
        reports: &[],
        profile_extensions: &[],
    };

    let mut packets = RtcpPacketVec::new();
    build_sr(&NinetyKhzClock, &stats, &mut packets);

    let RtcpPacket::SenderReport(report) = packets.pop().expect("should have packet") else {
        panic!("expected SR")
    };

    assert_eq!(report.sender_info.rtp_timestamp, 0xDEAD_BEEF);
}
