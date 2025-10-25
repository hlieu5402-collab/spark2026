#![doc = r#"
# spark-impl-tck

## 章节定位（Why）
- **目标**：为传输实现提供最小可运行的 TCK（Transport Compatibility Kit），确保每个传输模块在引入真实逻辑后立即被回归验证覆盖。
- **当前阶段**：聚焦 UDP 通道的首发路径，包括基础收发与 SIP `rport` 行为验证。

## 结构概览（How）
- `transport` 模块收纳各项传输相关测试；本阶段提供 `udp_smoke` 与 `udp_rport_return` 两组测试用例。
- 每个测试均使用 Tokio 多线程运行时，模拟客户端与服务器之间的报文交互。
"#]

/// 传输相关测试集合。
#[cfg(test)]
pub mod transport {
    use std::{net::SocketAddr, str};

    use anyhow::{Context, Result};
    use tokio::net::UdpSocket;

    use spark_core::transport::TransportSocketAddr;
    use spark_transport_udp::{SipViaRportDisposition, UdpEndpoint};

    /// 将 `TransportSocketAddr` 转换为标准库 `SocketAddr`，以便测试客户端套接字使用。
    fn transport_to_std(addr: TransportSocketAddr) -> SocketAddr {
        match addr {
            TransportSocketAddr::V4 { addr, port } => SocketAddr::from((addr, port)),
            TransportSocketAddr::V6 { addr, port } => {
                SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::from(addr)), port)
            }
            _ => panic!("测试暂不支持额外的 TransportSocketAddr 变体"),
        }
    }

    /// UDP 基线能力测试集合。
    pub mod udp_smoke {
        use super::{
            Context, Result, TransportSocketAddr, UdpEndpoint, UdpSocket, transport_to_std,
        };

        /// UDP 烟囱测试：验证绑定、收包、回包等最小链路是否可用。
        ///
        /// # 测试逻辑（How）
        /// 1. 绑定服务端 `UdpEndpoint` 到 `127.0.0.1:0`，并获取实际监听地址。
        /// 2. 客户端发送 `ping` 报文，服务端读取并校验源地址。
        /// 3. 使用 `UdpReturnRoute` 将 `pong` 报文回发客户端，确认响应可达。
        #[tokio::test(flavor = "multi_thread")]
        async fn bind_send_recv() -> Result<()> {
            let endpoint = UdpEndpoint::bind(TransportSocketAddr::V4 {
                addr: [127, 0, 0, 1],
                port: 0,
            })
            .await?;
            let server_addr = transport_to_std(endpoint.local_addr()?);

            let client = UdpSocket::bind("127.0.0.1:0").await?;
            let client_addr = client.local_addr()?;
            let payload = b"ping";

            client
                .send_to(payload, server_addr)
                .await
                .context("客户端发送 ping 失败")?;

            let mut buffer = vec![0u8; 1024];
            let incoming = endpoint
                .recv_from(&mut buffer)
                .await
                .context("服务端接收 ping 失败")?;

            assert_eq!(&buffer[..incoming.len()], payload);
            assert_eq!(incoming.peer().port(), client_addr.port());

            let route = incoming.return_route().clone();
            let response = b"pong";
            endpoint
                .send_to(response, &route)
                .await
                .context("服务端发送 pong 失败")?;

            let mut recv_buffer = vec![0u8; 1024];
            let (received, addr) = client
                .recv_from(&mut recv_buffer)
                .await
                .context("客户端接收 pong 失败")?;

            assert_eq!(addr, server_addr);
            assert_eq!(&recv_buffer[..received], response);
            assert_eq!(route.target().port(), client_addr.port());

            Ok(())
        }
    }

    /// 验证 `rport` 在响应中被正确回写，确保 NAT 场景可达。
    ///
    /// # 测试逻辑
    /// 1. 构造包含 `Via` 头且携带 `;rport` 参数的 SIP 请求。
    /// 2. 服务端解析后回发响应，`send_to` 应在 `Via` 头中补写客户端实际端口。
    /// 3. 客户端收到响应后，校验 `;rport=<port>` 是否存在。
    #[tokio::test(flavor = "multi_thread")]
    async fn udp_rport_return() -> Result<()> {
        let endpoint = UdpEndpoint::bind(TransportSocketAddr::V4 {
            addr: [127, 0, 0, 1],
            port: 0,
        })
        .await?;
        let server_addr = transport_to_std(endpoint.local_addr()?);

        let client = UdpSocket::bind("127.0.0.1:0").await?;
        let client_addr = client.local_addr()?;

        let request = "REGISTER sip:spark.invalid SIP/2.0\r\nVia: SIP/2.0/UDP client.invalid;branch=z9hG4bK-1;rport\r\n\r\n";
        client
            .send_to(request.as_bytes(), server_addr)
            .await
            .context("客户端发送 SIP 请求失败")?;

        let mut buffer = vec![0u8; 1024];
        let incoming = endpoint
            .recv_from(&mut buffer)
            .await
            .context("服务端接收 SIP 请求失败")?;

        assert_eq!(
            incoming.return_route().sip_rport(),
            SipViaRportDisposition::Requested
        );

        let route = incoming.return_route().clone();
        let response =
            "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP client.invalid;branch=z9hG4bK-1;rport\r\n\r\n";
        endpoint
            .send_to(response.as_bytes(), &route)
            .await
            .context("服务端发送 SIP 响应失败")?;

        let mut recv_buffer = vec![0u8; 1024];
        let (received, _) = client
            .recv_from(&mut recv_buffer)
            .await
            .context("客户端接收 SIP 响应失败")?;

        let text = str::from_utf8(&recv_buffer[..received])?;
        let expected_marker = format!(";rport={}", client_addr.port());
        assert!(
            text.contains(&expected_marker),
            "响应缺少正确的 rport，期望片段：{}，实际：{}",
            expected_marker,
            text
        );

        Ok(())
    }
}

/// RTP 相关契约测试集合。
#[cfg(test)]
pub mod rtp {
    use anyhow::Result;

    use spark_codec_rtp::{
        RTP_HEADER_MIN_LEN, RTP_VERSION, RtpHeader, RtpPacketBuilder, parse_rtp, seq_less,
    };
    use spark_core::buffer::{BufView, Chunks};

    /// 提供可复用的多分片 `BufView`，用于验证零拷贝解析在非连续缓冲下的行为。
    ///
    /// - **Why**：RTP 报文往往来自底层网络栈的 scatter/gather 缓冲，测试需覆盖多分片场景。
    /// - **How**：持有若干指向原始缓冲的切片指针，每次 `as_chunks` 时复制指针集合构造 `Chunks`。
    struct MultiChunkView<'a> {
        chunks: &'a [&'a [u8]],
        total_len: usize,
    }

    impl<'a> MultiChunkView<'a> {
        fn new(chunks: &'a [&'a [u8]]) -> Self {
            let total_len = chunks.iter().map(|chunk| chunk.len()).sum();
            Self { chunks, total_len }
        }
    }

    impl BufView for MultiChunkView<'_> {
        fn as_chunks(&self) -> Chunks<'_> {
            let slices: Vec<&[u8]> = self.chunks.to_vec();
            Chunks::from_vec(slices)
        }

        fn len(&self) -> usize {
            self.total_len
        }
    }

    /// RTP 头解析相关测试。
    pub mod header_parse {
        use super::*;
        use core::ptr;

        /// 解析带有 CSRC、扩展与 padding 的 RTP 报文，并验证零拷贝窗口。
        #[test]
        fn csrc_extension_padding_roundtrip() -> Result<()> {
            let mut header = RtpHeader::default();
            header.marker = true;
            header.payload_type = 96;
            header.sequence_number = 0xfffe;
            header.timestamp = 0x1122_3344;
            header.ssrc = 0x5566_7788;
            header.extension = true;
            header.padding = true;
            header
                .set_csrcs(&[0x0102_0304, 0x0506_0708])
                .expect("CSRC 设置失败");

            let payload: &[u8] = b"media-payload-demo";
            let extension_bytes = [0xAA, 0xBB, 0xCC, 0xDD, 0x11, 0x22, 0x33, 0x44];

            let builder = RtpPacketBuilder::new(header.clone())
                .payload_view(&payload)
                .extension_bytes(0x1001, &extension_bytes)
                .expect("扩展配置失败")
                .padding(4);

            let mut packet_bytes = vec![0u8; RTP_HEADER_MIN_LEN + 64];
            let used = builder
                .encode_into(&mut packet_bytes)
                .expect("RTP 编码失败");
            packet_bytes.truncate(used);

            // 刻意拆分为多分片，覆盖零拷贝解析。
            let chunk_a = &packet_bytes[..5];
            let chunk_b = &packet_bytes[5..20];
            let chunk_c = &packet_bytes[20..];
            let chunks: [&[u8]; 3] = [chunk_a, chunk_b, chunk_c];
            let view = MultiChunkView::new(&chunks);

            let packet = parse_rtp(&view).expect("RTP 解析失败");

            let parsed = packet.header();
            assert_eq!(parsed.version, RTP_VERSION);
            assert!(parsed.marker);
            assert_eq!(parsed.payload_type, 96);
            assert_eq!(parsed.sequence_number, 0xfffe);
            assert_eq!(parsed.timestamp, 0x1122_3344);
            assert_eq!(parsed.ssrc, 0x5566_7788);
            assert_eq!(parsed.csrcs(), &[0x0102_0304, 0x0506_0708]);

            let extension = packet.extension().expect("解析结果缺少 header 扩展视图");
            assert_eq!(extension.profile, 0x1001);
            let ext_data: Vec<u8> =
                extension
                    .data
                    .as_chunks()
                    .fold(Vec::new(), |mut acc, chunk| {
                        acc.extend_from_slice(chunk);
                        acc
                    });
            assert_eq!(ext_data, extension_bytes);

            let payload_section = packet.payload();
            assert_eq!(payload_section.len(), payload.len());
            let payload_chunks: Vec<&[u8]> = payload_section.as_chunks().collect();
            assert_eq!(payload_chunks.len(), 1, "payload 预计为单片");

            let payload_start = packet_bytes.len() - packet.padding_len() as usize - payload.len();
            let payload_slice = &packet_bytes[payload_start..payload_start + payload.len()];
            assert!(ptr::eq(payload_chunks[0].as_ptr(), payload_slice.as_ptr()));
            assert_eq!(payload_chunks[0], payload);

            assert_eq!(packet.padding_len(), 4);

            Ok(())
        }

        /// 当 header 与 builder 配置不一致时应返回错误，避免输出非法报文。
        #[test]
        fn builder_rejects_mismatched_flags() {
            let mut header = RtpHeader::default();
            header.padding = false;
            let builder = RtpPacketBuilder::new(header).padding(4);
            let mut buffer = vec![0u8; 64];
            let err = builder
                .encode_into(&mut buffer)
                .expect_err("padding 契约应触发错误");
            assert!(matches!(
                err,
                spark_codec_rtp::RtpEncodeError::HeaderMismatch(_)
            ));
        }
    }

    /// 序列号回绕比较测试。
    pub mod seq_wrap_around {
        use super::*;

        /// 验证常规递增场景与回绕场景的比较结果。
        #[test]
        fn ordering_rules() {
            assert!(seq_less(10, 11));
            assert!(!seq_less(11, 11));
            assert!(seq_less(0xFFFE, 0xFFFF));
            assert!(seq_less(0xFFFF, 0));
            assert!(!seq_less(0, 0x8000));
            assert!(!seq_less(0x8000, 0));
        }
    }
}
