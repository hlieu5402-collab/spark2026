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
    use std::{
        net::SocketAddr,
        str,
        sync::{Arc, Once},
    };

    use anyhow::{Context, Result};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream, UdpSocket},
        sync::oneshot,
    };

    use spark_core::transport::TransportSocketAddr;
    use spark_transport_tls::HotReloadingServerConfig;
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

    /// 确保 AWS-LC 作为 rustls 的全局加密后端。
    ///
    /// # 设计动机（Why）
    /// - Rustls 0.23 需要调用方显式安装 `CryptoProvider`；否则在首次构建 `ServerConfig`
    ///   时会 panic 并提示“无法自动选择 provider”。
    /// - 为了让所有测试逻辑共享同一初始化，我们在 `Once` 中调用安装逻辑，避免重复注册。
    ///
    /// # 契约说明（What）
    /// - **前置条件**：无；函数可被安全地多次调用。
    /// - **后置条件**：若 AWS-LC 可用，将成为进程级默认 provider；若安装失败则立即 panic，
    ///   该失败意味着二进制缺少编译期特性，属于环境配置错误。
    ///
    /// # 实现策略（How）
    /// - 利用 `Once` 确保只调用 `install_default` 一次；
    /// - 选择 AWS-LC（`aws-lc-rs` feature）作为 provider，保证与项目在 CI 中的配置一致。
    fn ensure_crypto_provider() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .expect("AWS-LC provider 注册失败，请检查 rustls 特性开关");
        });
    }

    /// 生成自签名服务端配置，并返回证书字节以供客户端信任。
    ///
    /// # 设计动机（Why）
    /// - 测试环境无法依赖外部分发的证书，因此需动态构造简易 PKI；
    /// - 同时返回证书原始字节，便于客户端 Root Store 校验及后续断言。
    ///
    /// # 契约说明（What）
    /// - **参数**：`common_name` 作为证书 CN 及 SAN，用于 SNI 校验；
    /// - **返回值**：`(Arc<ServerConfig>, Vec<u8>)`，分别为握手配置与证书 DER；
    /// - **前置条件**：仅用于测试场景，不具备生产级安全属性。
    ///
    /// # 实现概要（How）
    /// - 借助 `rcgen` 生成自签名证书与 PKCS#8 私钥；
    /// - 使用 `rustls` 构建 `ServerConfig`，配置为无需客户端证书；
    /// - 返回 `Arc` 封装的配置，便于直接注入热更容器。
    fn generate_server_config(common_name: &str) -> Result<(Arc<rustls::ServerConfig>, Vec<u8>)> {
        use std::convert::TryFrom;

        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
        use rustls::{
            ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer},
        };

        ensure_crypto_provider();

        let mut params =
            CertificateParams::new(vec![common_name.to_string()]).context("构造证书参数失败")?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        params.distinguished_name = dn;

        let key_pair = KeyPair::generate().context("生成证书私钥失败")?;
        let certificate = params
            .self_signed(&key_pair)
            .context("签发自签名证书失败")?;
        let cert_der = certificate.der().to_vec();
        let key_der = key_pair.serialize_der();

        let rustls_cert = CertificateDer::from(cert_der.clone()).into_owned();
        let private_key = PrivateKeyDer::try_from(key_der.clone())
            .map_err(|err| anyhow::anyhow!("解析私钥失败: {err}"))?;

        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![rustls_cert], private_key)
            .context("构建服务端 TLS 配置失败")?;

        Ok((Arc::new(server_config), cert_der))
    }

    /// 构造仅信任指定证书的客户端配置。
    ///
    /// # 契约（What）
    /// - **参数**：`certificate` 为服务端证书的 DER 编码；
    /// - **返回值**：包含单一根证书的 `ClientConfig`；
    /// - **前置条件**：证书需为自签名或可信链的根节点；
    /// - **后置条件**：生成的配置仅会信任该证书，适合测试差异化握手结果。
    ///
    /// # 实现（How）
    /// - 向 `RootCertStore` 写入证书 DER；
    /// - 通过 `rustls::ClientConfig::builder` 生成配置，并关闭客户端证书认证。
    fn build_client_config(certificate: &[u8]) -> Result<Arc<rustls::ClientConfig>> {
        use rustls::pki_types::CertificateDer;
        use rustls::{ClientConfig, RootCertStore};

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(certificate.to_vec()))
            .context("将证书写入 Root Store 失败")?;

        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(Arc::new(config))
    }

    /// TLS 证书热更新测试：验证新握手感知最新配置，旧连接保持可用。
    ///
    /// # 测试策略（How）
    /// 1. 使用 `HotReloadingServerConfig` 初始化第一版证书并启动监听；
    /// 2. 建立首个 TLS 连接，校验服务端证书指纹并完成回环收发；
    /// 3. 热替换第二版证书；新连接应观测到新证书，而旧连接继续收发不受影响。
    #[tokio::test(flavor = "multi_thread")]
    async fn tls_handshake_hotreload() -> Result<()> {
        use anyhow::Context;
        use rustls::pki_types::ServerName;
        use tokio_rustls::TlsConnector;

        let (initial_config, initial_cert) =
            generate_server_config("localhost").context("初始化首个证书失败")?;
        let (next_config, next_cert) =
            generate_server_config("localhost").context("初始化第二个证书失败")?;

        let hot_reload = HotReloadingServerConfig::new(initial_config);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("监听 TLS 端口失败")?;
        let listen_addr = listener.local_addr().context("读取监听地址失败")?;

        let (first_ready_tx, first_ready_rx) = oneshot::channel();
        let (second_ready_tx, second_ready_rx) = oneshot::channel();
        let server_hot_reload = hot_reload.clone();
        let server_handle = tokio::spawn(async move {
            let hot_reload = server_hot_reload;
            let mut tasks = Vec::new();

            let (stream_one, _) = listener.accept().await.context("接受首个 TLS 连接失败")?;
            tasks.push(tokio::spawn(handle_connection(
                hot_reload.clone(),
                stream_one,
                first_ready_tx,
            )));

            let (stream_two, _) = listener.accept().await.context("接受第二个 TLS 连接失败")?;
            tasks.push(tokio::spawn(handle_connection(
                hot_reload.clone(),
                stream_two,
                second_ready_tx,
            )));

            for task in tasks {
                task.await.context("服务端握手任务异常退出")??;
            }

            Ok::<(), anyhow::Error>(())
        });

        let server_name = ServerName::try_from("localhost").context("解析 ServerName 失败")?;

        let client_one_config = build_client_config(&initial_cert)?;
        let connector_one = TlsConnector::from(client_one_config);
        let tcp_one = TcpStream::connect(listen_addr)
            .await
            .context("建立首个 TCP 连接失败")?;
        let mut tls_one = connector_one
            .connect(server_name.clone(), tcp_one)
            .await
            .context("首个 TLS 握手失败")?;
        first_ready_rx.await.context("首个握手完成信号丢失")?;
        assert_server_cert(&tls_one, &initial_cert, "首个握手应返回初始证书");

        tls_one
            .write_all(b"v1-ping")
            .await
            .context("首个连接写入失败")?;
        tls_one.flush().await.context("首个连接刷新失败")?;
        let mut first_echo = vec![0u8; 7];
        tls_one
            .read_exact(&mut first_echo)
            .await
            .context("首个连接读取失败")?;
        assert_eq!(&first_echo, b"v1-ping");

        hot_reload.replace(next_config);

        let client_two_config = build_client_config(&next_cert)?;
        let connector_two = TlsConnector::from(client_two_config);
        let tcp_two = TcpStream::connect(listen_addr)
            .await
            .context("建立第二个 TCP 连接失败")?;
        let mut tls_two = connector_two
            .connect(server_name.clone(), tcp_two)
            .await
            .context("第二个 TLS 握手失败")?;
        second_ready_rx.await.context("第二个握手完成信号丢失")?;
        assert_server_cert(&tls_two, &next_cert, "热更后握手应返回新证书");

        tls_two
            .write_all(b"v2-ping")
            .await
            .context("第二个连接写入失败")?;
        tls_two.flush().await.context("第二个连接刷新失败")?;
        let mut second_echo = vec![0u8; 7];
        tls_two
            .read_exact(&mut second_echo)
            .await
            .context("第二个连接读取失败")?;
        assert_eq!(&second_echo, b"v2-ping");

        tls_one
            .write_all(b"still-ok")
            .await
            .context("旧连接写入失败")?;
        tls_one.flush().await.context("旧连接刷新失败")?;
        let mut legacy_echo = vec![0u8; 8];
        tls_one
            .read_exact(&mut legacy_echo)
            .await
            .context("旧连接读取失败")?;
        assert_eq!(&legacy_echo, b"still-ok");

        tls_one.shutdown().await.context("关闭旧连接失败")?;
        tls_two.shutdown().await.context("关闭新连接失败")?;

        server_handle.await.context("监听任务 join 失败")??;

        Ok(())
    }

    /// 读取 `TlsStream` 中的服务端证书并断言首张证书与期望值一致。
    fn assert_server_cert(
        stream: &tokio_rustls::client::TlsStream<TcpStream>,
        expected: &[u8],
        msg: &str,
    ) {
        let (_, common) = stream.get_ref();
        let chain = common
            .peer_certificates()
            .expect("握手尚未产生服务端证书链");
        assert_eq!(chain[0].as_ref(), expected, "{}", msg);
    }

    /// 服务端连接处理：负责 TLS 握手并回显客户端数据。
    async fn handle_connection(
        configs: HotReloadingServerConfig,
        stream: TcpStream,
        ready: oneshot::Sender<()>,
    ) -> Result<()> {
        let mut tls_stream = configs
            .accept(stream)
            .await
            .context("服务端 TLS 握手失败")?;
        let _ = ready.send(());

        let mut buffer = vec![0u8; 1024];
        loop {
            let read = tls_stream
                .read(&mut buffer)
                .await
                .context("服务端读取失败")?;
            if read == 0 {
                tls_stream.shutdown().await.context("服务端关闭连接失败")?;
                break;
            }
            tls_stream
                .write_all(&buffer[..read])
                .await
                .context("服务端写入失败")?;
            tls_stream.flush().await.context("服务端刷新失败")?;
        }

        Ok(())
    }
}
