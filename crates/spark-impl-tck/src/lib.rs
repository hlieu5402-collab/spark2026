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

            Ok(())
        }
    }
}
