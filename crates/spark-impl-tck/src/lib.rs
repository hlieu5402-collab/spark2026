#![doc = r#"
# spark-impl-tck

## 章节定位（Why）
- **目标**：为传输实现提供最小可运行的 TCK（Transport Compatibility Kit），确保每个传输模块在引入真实逻辑后立即被回归验证覆盖。
- **当前阶段**：聚焦 UDP 通道的首发路径，包括基础收发与 SIP `rport` 行为验证。

## 结构概览（How）
- `transport` 模块收纳各项传输相关测试：`udp_smoke`/`udp_rport_return` 覆盖 UDP，`tls_handshake`/`tls_alpn_route`
  验证 TLS 握手、SNI 选证与 ALPN 透出。
- 每个测试均使用 Tokio 多线程运行时，模拟客户端与服务器之间的报文交互。
"#]

pub(crate) mod placeholder {}

#[cfg(test)]
mod transport {
    use spark_core::{contract::CallContext, transport::TransportSocketAddr};
    use spark_transport_tcp::{ShutdownDirection, TcpChannel, TcpListener, TcpSocketConfig};
    use std::{net::SocketAddr, time::Duration};
    use tokio::time::sleep;

    /// 验证 TCP 通道在优雅关闭时遵循“FIN→等待 EOF→释放”的顺序，并正确应用 `linger` 配置。
    ///
    /// # 教案级注释
    ///
    /// ## 意图（Why）
    /// - 确保 `TcpChannel::close_graceful` 能在调用 `shutdown(Write)` 后等待对端发送 FIN，
    ///   与契约文档一致；
    /// - 校验 `TcpSocketConfig::with_linger(Some(..))` 在建连阶段确实生效，避免在生产
    ///   环境中因配置缺失导致 RST 行为不确定。
    ///
    /// ## 体系位置（Architecture）
    /// - 测试位于实现 TCK 的 crate 中，模拟“客户端主动关闭、服务端稍后响应 FIN”场景，
    ///   作为传输层的守门测试；
    /// - 运行时依赖 Tokio 多线程执行器，以贴近真实部署环境。
    ///
    /// ## 核心逻辑（How）
    /// - 启动监听器并接受连接；
    /// - 客户端使用自定义 `linger` 建立连接并调用 `close_graceful`；
    /// - 服务端在读取到 EOF 后延迟一段时间再关闭写半部，
    ///   断言客户端在此期间保持挂起；
    /// - 最终断言 `close_graceful` 成功完成且 `linger` 读取结果与配置一致。
    ///
    /// ## 契约（What）
    /// - **前置条件**：测试环境允许绑定环回地址并启动 Tokio 运行时；
    /// - **后置条件**：若任何步骤违反契约，测试将 panic，从而阻止回归通过。
    ///
    /// ## 注意事项（Trade-offs）
    /// - `sleep(150ms)` 模拟服务端清理资源的耗时，实测中可根据环境调整；
    /// - `SO_LINGER` 在 Linux 上以秒为单位，此处选择 1 秒避免取整误差。
    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_graceful_half_close() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("invalid bind addr");
        let listener = TcpListener::bind(TransportSocketAddr::from(bind_addr))
            .await
            .expect("bind listener");
        let local_addr = listener.local_addr();

        let server_ctx = CallContext::builder().build();
        let server_task_ctx = server_ctx.clone();

        let server_task = tokio::spawn(async move {
            let (server_channel, _) = listener
                .accept(&server_task_ctx)
                .await
                .expect("accept connection");
            let mut sink = [0u8; 1];
            let bytes = server_channel
                .read(&server_task_ctx, &mut sink)
                .await
                .expect("read client FIN");
            assert_eq!(bytes, 0, "server must observe client FIN");
            sleep(Duration::from_millis(150)).await;
            server_channel
                .shutdown(&server_task_ctx, ShutdownDirection::Write)
                .await
                .expect("shutdown write half");
        });

        let client_ctx = CallContext::builder().build();
        let client_config = TcpSocketConfig::new().with_linger(Some(Duration::from_secs(1)));
        let client_channel =
            TcpChannel::connect_with_config(&client_ctx, local_addr, client_config.clone())
                .await
                .expect("connect client");

        assert_eq!(
            client_channel.linger().await.expect("query linger option"),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            client_channel.config().linger(),
            client_config.linger(),
            "config cache should match applied linger",
        );

        let close_ctx = client_ctx.clone();
        let closing_channel = client_channel.clone();
        let close_task =
            tokio::spawn(async move { closing_channel.close_graceful(&close_ctx).await });

        sleep(Duration::from_millis(50)).await;
        assert!(
            !close_task.is_finished(),
            "close_graceful must wait for peer EOF",
        );

        server_task.await.expect("server task join");

        close_task
            .await
            .expect("close task join")
            .expect("close graceful result");

        drop(client_channel);
/// 传输相关测试集合。
#[cfg(test)]
pub mod transport {
    use std::{net::SocketAddr, str, sync::Arc};

    use anyhow::{Context, Result, anyhow};
    use rcgen::generate_simple_self_signed;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpStream as TokioTcpStream, UdpSocket},
    };
    use tokio_rustls::TlsConnector;

    use spark_core::{contract::CallContext, transport::TransportSocketAddr};
    use spark_transport_tcp::TcpListener;
    use spark_transport_tls::TlsAcceptor;
    use spark_transport_udp::{SipViaRportDisposition, UdpEndpoint};

    use rustls::crypto::aws_lc_rs::sign::any_supported_type;
    use rustls::{
        RootCertStore,
        client::ClientConfig,
        pki_types::{CertificateDer, PrivateKeyDer, ServerName},
        server::{ResolvesServerCertUsingSni, ServerConfig},
        sign::CertifiedKey,
    };

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

    /// TLS 服务器端观察到的握手与收发信息。
    #[derive(Debug, Default)]
    struct ServerObservation {
        payload: Vec<u8>,
        server_name: Option<String>,
        alpn: Option<Vec<u8>>,
    }

    fn generate_certified_key(host: &str) -> Result<(CertifiedKey, CertificateDer<'static>)> {
        let cert = generate_simple_self_signed([host.to_string()]).context("生成自签名证书失败")?;
        let cert_der =
            CertificateDer::from(cert.serialize_der().context("序列化证书 DER 失败")?).into_owned();
        let key_der = PrivateKeyDer::try_from(cert.serialize_private_key_der().as_slice())
            .map_err(|_| anyhow::anyhow!("私钥格式不受支持"))?
            .clone_key();
        let signing_key = any_supported_type(&key_der).context("构造签名密钥失败")?;
        let certified = CertifiedKey::new(vec![cert_der.clone()], signing_key);
        Ok((certified, cert_der))
    }

    fn build_server_config() -> Result<(
        Arc<ServerConfig>,
        CertificateDer<'static>,
        CertificateDer<'static>,
    )> {
        let (alpha_key, alpha_cert) = generate_certified_key("alpha.test")?;
        let (beta_key, beta_cert) = generate_certified_key("beta.test")?;
        let mut resolver = ResolvesServerCertUsingSni::new();
        resolver
            .add("alpha.test", alpha_key)
            .context("注册 alpha.test 证书失败")?;
        resolver
            .add("beta.test", beta_key)
            .context("注册 beta.test 证书失败")?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(resolver));
        config.alpn_protocols = vec![b"h3".to_vec(), b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok((Arc::new(config), alpha_cert, beta_cert))
    }

    fn build_client_config(
        cert: &CertificateDer<'static>,
        protocols: &[&str],
    ) -> Result<Arc<ClientConfig>> {
        let mut roots = RootCertStore::empty();
        roots
            .add(cert.clone())
            .context("将自签名证书加入 RootCertStore 失败")?;
        let mut config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = protocols.iter().map(|p| p.as_bytes().to_vec()).collect();
        Ok(Arc::new(config))
    }

    async fn perform_tls_round(
        listener: &TcpListener,
        acceptor: &TlsAcceptor,
        server_addr: SocketAddr,
        host: &str,
        client_config: Arc<ClientConfig>,
        request: &[u8],
        response: &[u8],
    ) -> Result<(ServerObservation, Vec<u8>)> {
        let server_future = async {
            let ctx = CallContext::builder().build();
            let (tcp, _) = listener
                .accept(&ctx)
                .await
                .map_err(|err| anyhow!("TLS 服务器接受连接失败: {err}"))?;
            let tls = acceptor
                .accept(&ctx, tcp)
                .await
                .map_err(|err| anyhow!("TLS 握手失败: {err}"))?;
            let mut buffer = vec![0u8; 512];
            let received = tls
                .read(&ctx, &mut buffer)
                .await
                .map_err(|err| anyhow!("TLS 服务端读取失败: {err}"))?;
            let mut observation = ServerObservation::default();
            observation.payload.extend_from_slice(&buffer[..received]);
            observation.server_name = tls.server_name().map(|name| name.to_string());
            observation.alpn = tls.alpn_protocol().map(|proto| proto.to_vec());
            tls.write(&ctx, response)
                .await
                .map_err(|err| anyhow!("TLS 服务端写入失败: {err}"))?;
            Ok::<_, anyhow::Error>(observation)
        };

        let client_future = async {
            let stream = TokioTcpStream::connect(server_addr)
                .await
                .context("客户端连接 TLS 服务器失败")?;
            let connector = TlsConnector::from(client_config);
            let name = ServerName::try_from(host.to_string()).context("无效的 SNI 主机名")?;
            let mut tls_stream = connector
                .connect(name, stream)
                .await
                .context("客户端 TLS 握手失败")?;
            tls_stream
                .write_all(request)
                .await
                .context("客户端写入请求失败")?;
            let mut buf = vec![0u8; 512];
            let read = tls_stream
                .read(&mut buf)
                .await
                .context("客户端读取响应失败")?;
            Ok::<_, anyhow::Error>(buf[..read].to_vec())
        };

        let (server_result, client_result) = tokio::join!(server_future, client_future);
        Ok((server_result?, client_result?))
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

    /// TLS 握手与读写行为验证。
    pub mod tls_handshake {
        use super::{
            Result, TcpListener, TlsAcceptor, TransportSocketAddr, anyhow, build_client_config,
            build_server_config, perform_tls_round, transport_to_std,
        };

        #[tokio::test(flavor = "multi_thread")]
        async fn sni_selection_and_io() -> Result<()> {
            let (server_config, alpha_cert, beta_cert) = build_server_config()?;
            let acceptor = TlsAcceptor::new(server_config);
            let listener = TcpListener::bind(TransportSocketAddr::V4 {
                addr: [127, 0, 0, 1],
                port: 0,
            })
            .await
            .map_err(|err| anyhow!("TLS 服务器绑定失败: {err}"))?;
            let server_addr = transport_to_std(listener.local_addr());

            let alpha_client = build_client_config(&alpha_cert, &[])?;
            let (obs_alpha, resp_alpha) = perform_tls_round(
                &listener,
                &acceptor,
                server_addr,
                "alpha.test",
                alpha_client,
                b"alpha-hello",
                b"alpha-world",
            )
            .await?;
            assert_eq!(obs_alpha.server_name.as_deref(), Some("alpha.test"));
            assert_eq!(obs_alpha.payload, b"alpha-hello");
            assert_eq!(resp_alpha, b"alpha-world");

            let beta_client = build_client_config(&beta_cert, &[])?;
            let (obs_beta, resp_beta) = perform_tls_round(
                &listener,
                &acceptor,
                server_addr,
                "beta.test",
                beta_client,
                b"beta-hello",
                b"beta-world",
            )
            .await?;
            assert_eq!(obs_beta.server_name.as_deref(), Some("beta.test"));
            assert_eq!(obs_beta.payload, b"beta-hello");
            assert_eq!(resp_beta, b"beta-world");

            Ok(())
        }
    }

    /// 验证 ALPN 协商结果是否对上层可见。
    pub mod tls_alpn_route {
        use super::{
            Result, TcpListener, TlsAcceptor, TransportSocketAddr, anyhow, build_client_config,
            build_server_config, perform_tls_round, transport_to_std,
        };

        #[tokio::test(flavor = "multi_thread")]
        async fn negotiated_protocol_exposed() -> Result<()> {
            let (server_config, alpha_cert, _) = build_server_config()?;
            let acceptor = TlsAcceptor::new(server_config);
            let listener = TcpListener::bind(TransportSocketAddr::V4 {
                addr: [127, 0, 0, 1],
                port: 0,
            })
            .await
            .map_err(|err| anyhow!("TLS 服务器绑定失败: {err}"))?;
            let server_addr = transport_to_std(listener.local_addr());

            let alpn_client = build_client_config(&alpha_cert, &["h2", "http/1.1"])?;
            let (observation, _) = perform_tls_round(
                &listener,
                &acceptor,
                server_addr,
                "alpha.test",
                alpn_client,
                b"alpn-req",
                b"alpn-resp",
            )
            .await?;

            assert_eq!(observation.server_name.as_deref(), Some("alpha.test"));
            assert_eq!(observation.alpn.as_deref(), Some(b"h2".as_ref()));
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
