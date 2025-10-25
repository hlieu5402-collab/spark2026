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

    /// 针对 UDP 批量 IO 优化路径的集成测试。
    pub mod udp_batch_io {
        use super::{Context, Result, UdpSocket};
        use spark_transport_udp::batch::{self, RecvBatchSlot, SendBatchSlot};

        /// 验证批量收发在多报文场景下的正确性与契约保持。
        ///
        /// # 测试步骤（How）
        /// 1. 客户端一次性发出三条报文，服务端调用 [`batch::recv_from`] 读取。
        /// 2. 检查槽位是否正确填充、来源地址是否与客户端一致、未发生截断。
        /// 3. 服务端构造对应响应，调用 [`batch::send_to`] 批量发回并校验写入长度。
        /// 4. 客户端逐一接收响应，确认报文内容与顺序匹配。
        #[tokio::test(flavor = "multi_thread")]
        async fn round_trip() -> Result<()> {
            let server = UdpSocket::bind("127.0.0.1:0")
                .await
                .context("服务端绑定失败")?;
            let server_addr = server.local_addr().context("获取服务端地址失败")?;

            let client = UdpSocket::bind("127.0.0.1:0")
                .await
                .context("客户端绑定失败")?;
            let client_addr = client.local_addr().context("获取客户端地址失败")?;

            let requests = [
                "batch-one".as_bytes(),
                "batch-two".as_bytes(),
                "batch-three".as_bytes(),
            ];
            for payload in &requests {
                client
                    .send_to(payload, server_addr)
                    .await
                    .context("客户端批量发送请求失败")?;
            }

            let mut recv_buffers: Vec<Vec<u8>> = vec![vec![0u8; 128]; requests.len()];
            let mut recv_slots: Vec<RecvBatchSlot<'_>> = recv_buffers
                .iter_mut()
                .map(|buf| RecvBatchSlot::new(buf.as_mut_slice()))
                .collect();

            let received = batch::recv_from(&server, &mut recv_slots)
                .await
                .context("服务端批量接收失败")?;
            assert_eq!(received, requests.len(), "应读取到全部请求报文");

            for (idx, slot) in recv_slots.iter().take(received).enumerate() {
                assert_eq!(slot.addr(), Some(client_addr));
                assert!(!slot.truncated(), "缓冲不应被截断");
                assert_eq!(slot.payload(), requests[idx]);
            }

            let expected_texts: Vec<String> =
                (0..received).map(|idx| format!("ack-{}", idx)).collect();
            let response_buffers: Vec<Vec<u8>> = expected_texts
                .iter()
                .map(|text| text.as_bytes().to_vec())
                .collect();
            let mut send_slots: Vec<SendBatchSlot<'_>> = response_buffers
                .iter()
                .zip(recv_slots.iter())
                .take(received)
                .map(|(payload, slot)| {
                    SendBatchSlot::new(payload.as_slice(), slot.addr().expect("应存在来源地址"))
                })
                .collect();

            batch::send_to(&server, &mut send_slots)
                .await
                .context("服务端批量发送失败")?;

            for (slot, payload) in send_slots.iter().zip(response_buffers.iter()) {
                assert_eq!(slot.sent(), payload.len());
            }

            for expected in &expected_texts {
                let mut buffer = vec![0u8; 128];
                let (len, addr) = client
                    .recv_from(&mut buffer)
                    .await
                    .context("客户端接收响应失败")?;
                assert_eq!(addr, server_addr);
                let text = std::str::from_utf8(&buffer[..len]).context("响应非 UTF-8")?;
                assert_eq!(text, expected);
            }

            Ok(())
        }
    }
}
