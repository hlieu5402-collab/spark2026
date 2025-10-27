use crate::{channel::QuicChannel, error, util::to_socket_addr};
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig};
use spark_core::prelude::{CoreError, TransportSocketAddr};

/// QUIC Endpoint 封装：统一监听与建连入口。
///
/// # 教案式注释
///
/// ## 意图（Why）
/// - **双角色支持**：一个 Endpoint 同时承担监听（Server）与建连（Client）职责，
///   与 QUIC 协议“一套 UDP Socket 多连接”的理念一致。
/// - **契约衔接**：对外暴露 `bind_server`/`bind_client`/`accept`/`connect` 四个入口，
///   以 `CoreError` 表示失败，确保与 `spark-core` 的错误体系兼容。
/// - **简化使用**：内部管理默认 `ClientConfig`，避免调用方在每次 `connect` 时显式传入。
///
/// ## 逻辑（How）
/// - 绑定阶段调用 `quinn::Endpoint::server/client` 创建底层 UDP Socket，映射 IO 错误；
/// - `accept` 使用 `Endpoint::accept()` 等待连接，随后将 `Connecting` 转换为
///   [`QuicConnection`]；
/// - `connect` 使用默认客户端配置发起握手，返回 `QuicConnection`；
/// - 每个连接再通过 [`QuicChannel`] 暴露多路复用流的读写能力。
///
/// ## 契约（What）
/// - `bind_server`/`bind_client`：返回 `QuicEndpoint`，失败时抛出结构化 `CoreError`；
/// - `accept`：仅服务器模式可用；若 Endpoint 已关闭，返回 `closed_error`；
/// - `connect`：客户端/服务器均可调用（便于自连测试），需要提前配置默认 ClientConfig；
/// - **前置条件**：必须在 Tokio 多线程运行时中调用；
/// - **后置条件**：返回的 [`QuicConnection`] 提供流级别操作，地址信息保持在 `TransportSocketAddr` 表示。
///
/// ## 风险与注意（Trade-offs）
/// - 当前实现仅支持单 `Endpoint` 下统一的默认客户端配置，若需按目的地动态切换，可在外层管理多个 Endpoint；
/// - `accept` 在 Endpoint 关闭后返回错误，调用方应处理；
/// - 本实现未对 0-RTT/重连做额外封装，需在调用层结合 `quinn` 配置进一步控制。
#[derive(Clone, Debug)]
pub struct QuicEndpoint {
    endpoint: Endpoint,
    mode: EndpointMode,
    local_addr: TransportSocketAddr,
}

#[derive(Clone, Debug)]
pub struct QuicConnection {
    connection: Connection,
    local_addr: TransportSocketAddr,
}

#[derive(Clone, Debug)]
enum EndpointMode {
    Server,
    Client,
}

impl QuicEndpoint {
    pub async fn bind_server(
        addr: TransportSocketAddr,
        server_config: ServerConfig,
        client_config: Option<ClientConfig>,
    ) -> spark_core::Result<Self, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let mut endpoint = Endpoint::server(server_config, socket_addr)
            .map_err(|err| error::map_io_error(error::BIND, err))?;
        if let Some(config) = client_config {
            endpoint.set_default_client_config(config);
        }
        let local_addr = endpoint
            .local_addr()
            .map_err(|err| error::map_io_error(error::BIND, err))?;
        Ok(Self {
            endpoint,
            mode: EndpointMode::Server,
            local_addr: local_addr.into(),
        })
    }

    pub async fn bind_client(
        addr: TransportSocketAddr,
        client_config: ClientConfig,
    ) -> spark_core::Result<Self, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let mut endpoint =
            Endpoint::client(socket_addr).map_err(|err| error::map_io_error(error::BIND, err))?;
        endpoint.set_default_client_config(client_config);
        let local_addr = endpoint
            .local_addr()
            .map_err(|err| error::map_io_error(error::BIND, err))?;
        Ok(Self {
            endpoint,
            mode: EndpointMode::Client,
            local_addr: local_addr.into(),
        })
    }

    pub async fn accept(&self) -> spark_core::Result<QuicConnection, CoreError> {
        match self.mode {
            EndpointMode::Server => {
                let incoming = self
                    .endpoint
                    .accept()
                    .await
                    .ok_or_else(|| error::closed_error(error::ACCEPT, "endpoint closed"))?;
                let connecting = incoming
                    .accept()
                    .map_err(|err| error::map_connection_error(error::ACCEPT, err))?;
                let connection = connecting
                    .await
                    .map_err(|err| error::map_connection_error(error::ACCEPT, err))?;
                Ok(QuicConnection::new(connection, self.local_addr))
            }
            EndpointMode::Client => Err(error::invalid_endpoint_mode(error::ACCEPT)),
        }
    }

    pub async fn connect(
        &self,
        addr: TransportSocketAddr,
        server_name: &str,
    ) -> spark_core::Result<QuicConnection, CoreError> {
        let socket_addr = to_socket_addr(addr);
        let connecting = self
            .endpoint
            .connect(socket_addr, server_name)
            .map_err(|err| error::map_connect_error(error::CONNECT, err))?;
        let connection = connecting
            .await
            .map_err(|err| error::map_connection_error(error::CONNECT, err))?;
        Ok(QuicConnection::new(connection, self.local_addr))
    }

    pub fn local_addr(&self) -> TransportSocketAddr {
        self.local_addr
    }
}

impl QuicConnection {
    fn new(connection: Connection, local_addr: TransportSocketAddr) -> Self {
        Self {
            connection,
            local_addr,
        }
    }

    pub fn peer_addr(&self) -> TransportSocketAddr {
        self.connection.remote_address().into()
    }

    pub fn local_addr(&self) -> TransportSocketAddr {
        self.local_addr
    }

    pub async fn open_bi(&self) -> spark_core::Result<QuicChannel, CoreError> {
        let (send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|err| error::map_connection_error(error::OPEN_STREAM, err))?;
        Ok(QuicChannel::from_streams(
            self.connection.clone(),
            send,
            recv,
            self.local_addr,
        ))
    }

    pub async fn accept_bi(&self) -> spark_core::Result<Option<QuicChannel>, CoreError> {
        match self.connection.accept_bi().await {
            Ok((send, recv)) => Ok(Some(QuicChannel::from_streams(
                self.connection.clone(),
                send,
                recv,
                self.local_addr,
            ))),
            Err(quinn::ConnectionError::LocallyClosed)
            | Err(quinn::ConnectionError::ConnectionClosed(_))
            | Err(quinn::ConnectionError::ApplicationClosed(_)) => Ok(None),
            Err(err) => Err(error::map_connection_error(error::OPEN_STREAM, err)),
        }
    }
}
