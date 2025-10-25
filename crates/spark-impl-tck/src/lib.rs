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

    /// QUIC 多路复用流读写回环测试，验证流 → Channel 映射与背压信号。
    pub mod quic_multiplex {
        use super::{Result, TransportSocketAddr};
        use anyhow::anyhow;
        use quinn::{ClientConfig, ServerConfig};
        use rcgen::generate_simple_self_signed;
        use rustls::RootCertStore;
        use rustls_pki_types::{CertificateDer, PrivateKeyDer};
        use spark_core::{
            context::ExecutionContext,
            contract::CallContext,
            error::CoreError,
            status::ready::{ReadyCheck, ReadyState},
        };
        use spark_transport_quic::{QuicEndpoint, ShutdownDirection};
        use std::{sync::Arc, task::Poll};

        fn build_tls_configs() -> Result<(ServerConfig, ClientConfig)> {
            let cert = generate_simple_self_signed(vec!["localhost".into()])?;
            let cert_der_raw = cert.serialize_der()?;
            let key_der_raw = cert.serialize_private_key_der();

            let cert_der = CertificateDer::from_slice(&cert_der_raw).into_owned();
            let mut roots = RootCertStore::empty();
            roots.add(cert_der.clone())?;

            let client_config = ClientConfig::with_root_certificates(Arc::new(roots))
                .map_err(|err| anyhow!(err))?;

            let key = PrivateKeyDer::try_from(key_der_raw.as_slice())
                .map_err(|err| anyhow!(err))?
                .clone_key();

            let server_config = ServerConfig::with_single_cert(vec![cert_der.clone()], key)
                .map_err(|err| anyhow!(err))?;

            Ok((server_config, client_config))
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn bidirectional_streams() -> Result<()> {
            let (server_cfg, client_cfg) = build_tls_configs()?;

            let server_endpoint = QuicEndpoint::bind_server(
                TransportSocketAddr::V4 {
                    addr: [127, 0, 0, 1],
                    port: 0,
                },
                server_cfg,
                Some(client_cfg.clone()),
            )
            .await
            .map_err(|err| anyhow!(err))?;
            let server_addr = server_endpoint.local_addr();

            let client_endpoint = QuicEndpoint::bind_client(
                TransportSocketAddr::V4 {
                    addr: [127, 0, 0, 1],
                    port: 0,
                },
                client_cfg,
            )
            .await
            .map_err(|err| anyhow!(err))?;

            let server_handle = tokio::spawn({
                let server = server_endpoint.clone();
                async move {
                    let connection = server.accept().await?;
                    while let Some(channel) = connection.accept_bi().await? {
                        let ctx = CallContext::builder().build();
                        let mut buffer = vec![0u8; 1024];
                        let size = channel.read(&ctx, &mut buffer).await?;
                        let mut response = buffer[..size].to_vec();
                        response
                            .iter_mut()
                            .for_each(|byte| *byte = byte.to_ascii_uppercase());
                        channel.write(&ctx, &response).await?;
                        channel.shutdown(&ctx, ShutdownDirection::Write).await?;
                    }
                    Ok::<(), CoreError>(())
                }
            });

            let connection = client_endpoint
                .connect(server_addr, "localhost")
                .await
                .map_err(|err| anyhow!(err))?;
            let payloads = vec![b"alpha".as_ref(), b"bravo".as_ref(), b"charlie".as_ref()];
            let mut responses = Vec::with_capacity(payloads.len());
            for payload in &payloads {
                let ctx = CallContext::builder().build();
                let exec_ctx = ExecutionContext::from(&ctx);
                let channel = connection.open_bi().await.map_err(|err| anyhow!(err))?;
                match channel.poll_ready(&exec_ctx) {
                    Poll::Ready(ReadyCheck::Ready(state)) => {
                        assert!(matches!(state, ReadyState::Ready | ReadyState::Busy(_)));
                    }
                    Poll::Ready(ReadyCheck::Err(err)) => return Err(anyhow!(err)),
                    Poll::Pending => panic!("poll_ready unexpectedly returned Pending"),
                    Poll::Ready(other) => panic!("unexpected ReadyCheck variant: {other:?}"),
                }
                channel
                    .write(&ctx, payload)
                    .await
                    .map_err(|err| anyhow!(err))?;
                channel
                    .shutdown(&ctx, ShutdownDirection::Write)
                    .await
                    .map_err(|err| anyhow!(err))?;
                let mut buffer = vec![0u8; 1024];
                let size = channel
                    .read(&ctx, &mut buffer)
                    .await
                    .map_err(|err| anyhow!(err))?;
                responses.push(buffer[..size].to_vec());
            }

            for (expected, actual) in payloads.iter().zip(responses.iter()) {
                let uppercase: Vec<u8> = expected.iter().map(|b| b.to_ascii_uppercase()).collect();
                assert_eq!(actual, &uppercase);
            }

            drop(connection);

            server_handle
                .await
                .map_err(|err| anyhow!(err))?
                .map_err(|err| anyhow!(err))?;

            Ok(())
        }
    }
}
