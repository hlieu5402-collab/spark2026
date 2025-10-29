//! RTCP 复合报文解析测试。
//!
//! # 教案定位（Why）
//! - 验证 `spark-codec-rtcp::parse_rtcp` 能够正确拆解 Sender Report、SDES、BYE 等典型组合。
//! - 同时校验错误分支（例如首分组不是报告类时应立即失败），确保实现与 RFC3550 的约束保持一致。
//!
//! # 覆盖范围（What）
//! - `parse_sr_sdes_bye_compound`：解析一个包含 SR、SDES、BYE 的复合报文，逐字段断言解析结果。
//! - `reject_compound_without_report`：验证首分组不是 SR/RR 时返回 `FirstPacketNotReport` 错误。
//!
//! # 测试技巧（How）
//! - 报文字节通过 Python 脚本生成，避免手动计算长度字段时出错；固定在常量中便于复用。
//! - 使用 `assert_eq!` 搭配结构体字段断言，保证解析输出的契约稳定可靠。

use spark_codec_rtcp::{
    Goodbye, RtcpError, RtcpPacket, SenderReport, SourceDescription, parse_rtcp,
};

/// 包含 SR + SDES + BYE 的复合 RTCP 报文。
const COMPOUND_PACKET: [u8; 96] = [
    0x81, 0xC8, 0x00, 0x0C, 0x01, 0x02, 0x03, 0x04, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
    0x99, 0xAA, 0xBB, 0xCC, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x01, 0x00, 0x0A, 0x0B, 0x0C, 0x0D,
    0x05, 0x00, 0x01, 0x02, 0x12, 0x34, 0x56, 0x78, 0x00, 0x00, 0x10, 0x00, 0x45, 0x67, 0x89, 0xAB,
    0x00, 0x00, 0x12, 0x34, 0x81, 0xCA, 0x00, 0x05, 0x01, 0x02, 0x03, 0x04, 0x01, 0x0A, 0x61, 0x6C,
    0x69, 0x63, 0x65, 0x40, 0x76, 0x6F, 0x69, 0x70, 0x00, 0x00, 0x00, 0x00, 0x81, 0xCB, 0x00, 0x04,
    0x0A, 0x0B, 0x0C, 0x0D, 0x08, 0x73, 0x68, 0x75, 0x74, 0x64, 0x6F, 0x77, 0x6E, 0x00, 0x00, 0x00,
];

/// 首分组非报告类型的最小化 RTCP 报文。
const BYE_ONLY: [u8; 4] = [0x80, 0xCB, 0x00, 0x00];

#[test]
fn parse_sr_sdes_bye_compound() {
    let packets = parse_rtcp(&COMPOUND_PACKET[..]).expect("解析 SR+SDES+BYE 复合报文应成功");
    assert_eq!(packets.len(), 3, "复合报文应包含 3 个分组");

    match &packets[0] {
        RtcpPacket::SenderReport(SenderReport {
            sender_ssrc,
            sender_info,
            reports,
            profile_extensions,
        }) => {
            assert_eq!(*sender_ssrc, 0x0102_0304);
            assert_eq!(sender_info.ntp_timestamp, 0x1122_3344_5566_7788);
            assert_eq!(sender_info.rtp_timestamp, 0x99AA_BBCC);
            assert_eq!(sender_info.sender_packet_count, 0x10);
            assert_eq!(sender_info.sender_octet_count, 0x100);
            assert_eq!(reports.len(), 1);
            let report = &reports[0];
            assert_eq!(report.source_ssrc, 0x0A0B_0C0D);
            assert_eq!(report.fraction_lost, 0x05);
            assert_eq!(report.cumulative_lost, 0x0000_0102);
            assert_eq!(report.extended_highest_sequence, 0x1234_5678);
            assert_eq!(report.interarrival_jitter, 0x0000_1000);
            assert_eq!(report.last_sr_timestamp, 0x4567_89AB);
            assert_eq!(report.delay_since_last_sr, 0x0000_1234);
            assert!(profile_extensions.is_empty(), "示例报文未携带扩展字段");
        }
        other => panic!("首分组应解析为 SenderReport，实际为 {other:?}"),
    }

    match &packets[1] {
        RtcpPacket::SourceDescription(SourceDescription { chunks }) => {
            assert_eq!(chunks.len(), 1);
            let chunk = &chunks[0];
            assert_eq!(chunk.source, 0x0102_0304);
            assert_eq!(chunk.items.len(), 1);
            let item = &chunk.items[0];
            assert_eq!(item.item_type, 0x01, "示例使用 CNAME 条目");
            let text = String::from_utf8(item.value.clone()).expect("CNAME 应为合法 UTF-8");
            assert_eq!(text, "alice@voip");
        }
        other => panic!("第二个分组应为 SDES，实际为 {other:?}"),
    }

    match &packets[2] {
        RtcpPacket::Goodbye(Goodbye { sources, reason }) => {
            assert_eq!(sources, &[0x0A0B_0C0D]);
            assert_eq!(reason.as_deref(), Some("shutdown"));
        }
        other => panic!("第三个分组应为 BYE，实际为 {other:?}"),
    }
}

#[test]
fn reject_compound_without_report() {
    let error = parse_rtcp(&BYE_ONLY[..]).expect_err("首分组不是报告时应触发错误");
    assert_eq!(
        error,
        RtcpError::FirstPacketNotReport { packet_type: 203 },
        "需返回明确的 FirstPacketNotReport 错误"
    );
}
