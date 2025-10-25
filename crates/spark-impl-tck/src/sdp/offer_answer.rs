//! Offer/Answer 协商及 DTMF 属性的兼容性测试。
//!
//! ## 设计意图（Why）
//! - 验证 `spark-codec-sdp` 提供的 `apply_offer_answer` 是否能够根据本地能力筛选 PCMU/PCMA；
//! - 确保当 offer 提供 RFC 4733 `telephone-event` 时，本地在开启能力后能正确解析 `a=rtpmap` 与 `a=fmtp`；
//! - 为后续引入更复杂的媒体协商逻辑打下回归测试基础。
//!
//! ## 结构安排（How）
//! - `basic_pcm`：单元测试本地同时支持 PCMU/PCMA 时的最小交集选择；
//! - `negotiate_dtmf`：验证在开启 DTMF 能力时解析电话事件负载与事件范围。

use spark_codec_sdp::{
    offer_answer::{AnswerCapabilities, AudioAnswer, AudioCaps, AudioCodec, apply_offer_answer},
    parse_sdp,
};

/// 当本地偏好 PCMU 且 offer 同时给出 PCMU/PCMA 时，应优先选择 PCMU。
///
/// ### 预期行为（What）
/// - 解析成功；
/// - 回答计划存在音频分支，并被接受；
/// - 选中负载类型为 0，编解码器为 PCMU。
#[test]
fn basic_pcm() {
    let offer = parse_sdp(
        "v=0\r\n\
         o=- 0 0 IN IP4 192.0.2.1\r\n\
         s=Test\r\n\
         t=0 0\r\n\
         m=audio 49170 RTP/AVP 0 8\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:8 PCMA/8000\r\n",
    )
    .expect("offer parsing");

    let caps = AnswerCapabilities::audio_only(AudioCaps::new(AudioCodec::Pcmu, true, true, false));

    let plan = apply_offer_answer(&offer, &caps);
    let audio_plan = plan.audio.expect("audio m-line must exist");

    match audio_plan {
        AudioAnswer::Accepted(accept) => {
            assert_eq!(accept.codec, AudioCodec::Pcmu);
            assert_eq!(accept.payload_type, 0);
            assert_eq!(accept.rtpmap.encoding, "PCMU");
            assert_eq!(accept.rtpmap.clock_rate, 8000);
            assert!(accept.telephone_event.is_none());
        }
        AudioAnswer::Rejected => panic!("audio should be accepted"),
    }
}

/// 当本地愿意协商 DTMF 时，应能够解析 `telephone-event` 的 `rtpmap` 与 `fmtp`。
///
/// ### 预期行为（What）
/// - 回答计划中的 DTMF 配置存在，事件范围与采样率与 offer 一致；
/// - 即便 offer 未提供 PCMA，本地也能在仅支持 PCMU 的情况下完成协商。
#[test]
fn negotiate_dtmf() {
    let offer = parse_sdp(
        "v=0\r\n\
         o=- 0 0 IN IP4 198.51.100.1\r\n\
         s=DTMF\r\n\
         t=0 0\r\n\
         m=audio 5004 RTP/AVP 0 101\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-15\r\n",
    )
    .expect("offer parsing");

    let caps = AnswerCapabilities::audio_only(AudioCaps::new(AudioCodec::Pcmu, true, false, true));

    let plan = apply_offer_answer(&offer, &caps);
    let audio_plan = plan.audio.expect("audio m-line must exist");

    match audio_plan {
        AudioAnswer::Accepted(accept) => {
            let dtmf = accept.telephone_event.expect("dtmf should be negotiated");
            assert_eq!(accept.codec, AudioCodec::Pcmu);
            assert_eq!(accept.payload_type, 0);
            assert_eq!(dtmf.payload_type, 101);
            assert_eq!(dtmf.clock_rate, 8000);
            assert_eq!(dtmf.events, "0-15");
        }
        AudioAnswer::Rejected => panic!("audio should be accepted"),
    }
}
