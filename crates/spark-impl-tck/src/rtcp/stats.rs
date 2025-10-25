//! SR/RR 报文生成测试集。
//!
//! # 教案意图（Why）
//! - 验证 `spark-codec-rtcp::build_sr` 与 `build_rr` 对时间映射、字段对齐与边界检查的实现是否符合 RFC3550。
//! - 帮助实现者理解 Sender Report/Receiver Report 的字节布局，在扩展复合报文生成之前锁定基线能力。
//!
//! # 测试范围（What）
//! - **正向路径**：生成包含统计与 Profile 扩展字段的 SR/RR，并比对完整字节序列；
//! - **异常路径**：验证时钟回退、扩展对齐与累计丢包范围等契约违规时返回错误。
//!
//! # 设计策略（How）
//! - 使用固定时间与统计输入，以纯字节断言避免实现与测试共享逻辑；
//! - 通过拆分测试函数覆盖不同的错误分支，确保错误信息可读且精确。

use std::time::{Duration, UNIX_EPOCH};

use spark_codec_rtcp::{
    BuildError, ReceiverStat, ReceptionStat, RtpClockMapper, SenderStat, build_rr, build_sr,
};

/// 生成包含单个接收报告与 Profile 扩展的 Sender Report，并校验完整字节序列。
#[test]
fn build_sender_report_with_statistics() {
    let clock = RtpClockMapper::new(UNIX_EPOCH, 0, 90_000);
    let capture_time = UNIX_EPOCH + Duration::from_millis(1500);
    let reports = [ReceptionStat {
        source_ssrc: 0x0102_0304,
        fraction_lost: 0x05,
        cumulative_lost: 0x0000_0607,
        extended_highest_sequence: 0x1020_3040,
        interarrival_jitter: 0x1122_3344,
        last_sr_timestamp: 0x5566_7788,
        delay_since_last_sr: 0x99AA_BBCC,
    }];
    let sender_stat = SenderStat {
        sender_ssrc: 0x5566_7788,
        capture_time,
        sender_packet_count: 0x0001_0203,
        sender_octet_count: 0x0002_0304,
        reports: &reports,
        profile_extensions: &[0xDE, 0xAD, 0xBE, 0xEF],
    };

    let mut out = Vec::new();
    build_sr(&clock, &sender_stat, &mut out).expect("SR 生成应成功");

    let expected = hex::decode(
        "81c8000d5566778883aa7e818000000000020f580001020300020304010203040500060710203040112233445566778899aabbccdeadbeef",
    )
    .expect("固定的十六进制字符串应合法");
    assert_eq!(out, expected);
}

/// 生成包含两个接收报告的 Receiver Report，并验证负累计丢包的编码逻辑。
#[test]
fn build_receiver_report_with_signed_loss() {
    let reports = [
        ReceptionStat {
            source_ssrc: 0x0102_0304,
            fraction_lost: 0x05,
            cumulative_lost: 0x0000_0607,
            extended_highest_sequence: 0x1020_3040,
            interarrival_jitter: 0x1122_3344,
            last_sr_timestamp: 0x5566_7788,
            delay_since_last_sr: 0x99AA_BBCC,
        },
        ReceptionStat {
            source_ssrc: 0x0A0B_0C0D,
            fraction_lost: 0xFE,
            cumulative_lost: -5,
            extended_highest_sequence: 0x2222_2222,
            interarrival_jitter: 0x3333_3333,
            last_sr_timestamp: 0x4444_4444,
            delay_since_last_sr: 0x5555_5555,
        },
    ];
    let recv_stat = ReceiverStat {
        reporter_ssrc: 0xCAFE_BABE,
        reports: &reports,
        profile_extensions: &[],
    };

    let mut out = Vec::new();
    build_rr(&recv_stat, &mut out).expect("RR 生成应成功");

    let expected = hex::decode(
        "82c9000dcafebabe010203040500060710203040112233445566778899aabbcc0a0b0c0dfefffffb22222222333333334444444455555555",
    )
    .expect("固定的十六进制字符串应合法");
    assert_eq!(out, expected);
}

/// 当采样时间早于映射器起点时应返回 `ClockWentBackwards` 错误，避免生成无效 NTP/RTP 时间戳。
#[test]
fn sr_rejects_clock_rewind() {
    let clock = RtpClockMapper::new(UNIX_EPOCH + Duration::from_secs(10), 0, 90_000);
    let sender_stat = SenderStat {
        sender_ssrc: 0x1234_5678,
        capture_time: UNIX_EPOCH,
        sender_packet_count: 0,
        sender_octet_count: 0,
        reports: &[],
        profile_extensions: &[],
    };

    let mut out = Vec::new();
    let error = build_sr(&clock, &sender_stat, &mut out).expect_err("采样时间回退应触发错误");
    assert!(matches!(error, BuildError::ClockWentBackwards));
}

/// Profile 扩展长度必须按 32-bit 对齐，否则返回 `MisalignedProfileExtension`。
#[test]
fn rr_rejects_misaligned_extension() {
    let reports = [];
    let recv_stat = ReceiverStat {
        reporter_ssrc: 0xCAFE_BABE,
        reports: &reports,
        profile_extensions: &[0xAA],
    };
    let mut out = Vec::new();
    let error = build_rr(&recv_stat, &mut out).expect_err("未对齐的扩展应触发错误");
    assert!(matches!(
        error,
        BuildError::MisalignedProfileExtension { len: 1 }
    ));
}

/// `cumulative_lost` 超过 24-bit 表示范围时应触发 `CumulativeLostOutOfRange` 错误。
#[test]
fn reception_stat_rejects_out_of_range_loss() {
    let reports = [ReceptionStat {
        source_ssrc: 0,
        fraction_lost: 0,
        cumulative_lost: 1 << 24,
        extended_highest_sequence: 0,
        interarrival_jitter: 0,
        last_sr_timestamp: 0,
        delay_since_last_sr: 0,
    }];
    let recv_stat = ReceiverStat {
        reporter_ssrc: 0,
        reports: &reports,
        profile_extensions: &[],
    };
    let mut out = Vec::new();
    let error = build_rr(&recv_stat, &mut out).expect_err("超范围累计丢包应触发错误");
    assert!(matches!(
        error,
        BuildError::CumulativeLostOutOfRange { value } if value == (1 << 24)
    ));
}
