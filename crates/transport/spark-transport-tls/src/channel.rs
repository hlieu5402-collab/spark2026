use spark_core::prelude::{CallContext, CoreError, ExecutionContext, TransportSocketAddr};
use std::{borrow::Cow, pin::Pin, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream as TokioTcpStream,
    sync::Mutex as AsyncMutex,
};
use tokio_rustls::server::TlsStream;

use crate::{
    error::{self, FLUSH},
    util::run_with_context,
};
use spark_transport::{
    BackpressureDecision, BackpressureMetrics, ShutdownDirection,
    TransportConnection as TransportConnectionTrait,
};

/// TLS 通道对象，封装握手后的加密读写能力。
///
/// # 教案级注释
///
/// ## 意图（Why）
/// - 为上层 Handler 提供与 `TcpChannel` 类似的读写 API，但内部通过 `rustls` 进行加解密；
/// - 暴露 SNI 与 ALPN 元数据，供路由与协议选择逻辑使用（例如区分 http/1.1、h2、h3）。
///
/// ## 逻辑（How）
/// - 以 `tokio::sync::Mutex` 包裹 `TlsStream`，确保多线程下的互斥访问；
/// - 所有 I/O 方法调用 `run_with_context` 注入取消/截止语义，并使用 `error::map_stream_error`
///   将底层错误映射为结构化 `CoreError`；
/// - 构造时读取 `ServerConnection` 内部的 `server_name` 与 `alpn_protocol`，缓存在结构体字段中。
///
/// ## 契约（What）
/// - `read`/`write`：单次加密读写操作，遵循 `CallContext` 的取消与截止约束；
/// - `shutdown`：发送 TLS `close_notify` 并刷新缓冲区；
/// - `peer_addr`/`local_addr`：返回原 TCP 连接的地址信息；
/// - `server_name`：客户端提供的 SNI（若有）；
/// - `alpn_protocol`：协商出的应用层协议标识（若有）。
///
/// ## 风险与权衡（Trade-offs）
/// - 当前实现未提供背压统计，若需要与 TCP 通道一致的 `poll_ready` 语义可在后续扩展；
/// - 为保持 API 简洁，`write` 仅保证将全部明文写入 TLS 会话，如需确认刷盘请结合 `shutdown`
///   或额外的应用级 ACK。
#[derive(Clone, Debug)]
pub struct TlsChannel {
    inner: Arc<TlsChannelInner>,
}

#[derive(Debug)]
struct TlsChannelInner {
    stream: AsyncMutex<TlsStream<TokioTcpStream>>,
    local_addr: TransportSocketAddr,
    peer_addr: TransportSocketAddr,
    server_name: Option<String>,
    alpn_protocol: Option<Vec<u8>>,
}

impl TlsChannel {
    pub(crate) fn new(
        stream: TlsStream<TokioTcpStream>,
        local_addr: TransportSocketAddr,
        peer_addr: TransportSocketAddr,
    ) -> Self {
        let (_, connection) = stream.get_ref();
        let server_name = connection.server_name().map(|name| name.to_string());
        let alpn_protocol = connection.alpn_protocol().map(|proto| proto.to_vec());

        Self {
            inner: Arc::new(TlsChannelInner {
                stream: AsyncMutex::new(stream),
                local_addr,
                peer_addr,
                server_name,
                alpn_protocol,
            }),
        }
    }

    /// 读取解密后的明文数据。
    ///
    /// # 契约说明
    /// - `ctx`: 需要遵循的取消/截止上下文；
    /// - `buf`: 目标缓冲区；
    /// - **返回值**：实际读取的字节数。
    pub async fn read(
        &self,
        ctx: &CallContext,
        buf: &mut [u8],
    ) -> spark_core::Result<usize, CoreError> {
        run_with_context(
            ctx,
            error::READ,
            async {
                let mut guard = self.inner.stream.lock().await;
                guard.read(buf).await
            },
            error::map_stream_error,
        )
        .await
    }

    /// 写入明文数据并由 TLS 层加密。
    pub async fn write(
        &self,
        ctx: &CallContext,
        buf: &[u8],
    ) -> spark_core::Result<usize, CoreError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len();
        run_with_context(
            ctx,
            error::WRITE,
            async {
                let mut guard = self.inner.stream.lock().await;
                guard.write_all(buf).await.map(|_| len)
            },
            error::map_stream_error,
        )
        .await
    }

    /// 刷新 TLS 会话缓冲区，确保待发送的密文全部写出。
    pub async fn flush(&self, ctx: &CallContext) -> spark_core::Result<(), CoreError> {
        run_with_context(
            ctx,
            FLUSH,
            async {
                let mut guard = self.inner.stream.lock().await;
                guard.flush().await
            },
            error::map_stream_error,
        )
        .await
    }

    /// 发送 TLS `close_notify` 并关闭写方向。
    pub async fn shutdown(&self, ctx: &CallContext) -> spark_core::Result<(), CoreError> {
        run_with_context(
            ctx,
            error::SHUTDOWN,
            async {
                let mut guard = self.inner.stream.lock().await;
                AsyncWriteExt::shutdown(&mut *guard).await
            },
            error::map_stream_error,
        )
        .await
    }

    /// 获取对端地址。
    pub fn peer_addr(&self) -> TransportSocketAddr {
        self.inner.peer_addr
    }

    /// 获取本地地址。
    pub fn local_addr(&self) -> TransportSocketAddr {
        self.inner.local_addr
    }

    /// 返回客户端提供的 SNI（若存在）。
    pub fn server_name(&self) -> Option<&str> {
        self.inner.server_name.as_deref()
    }

    /// 返回协商得到的 ALPN 标识（若存在）。
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.inner.alpn_protocol.as_deref()
    }
}

impl TransportConnectionTrait for TlsChannel {
    type Error = CoreError;
    type CallCtx<'ctx> = CallContext;
    type ReadyCtx<'ctx> = ExecutionContext<'ctx>;

    type ReadFuture<'ctx>
        = Pin<
        Box<dyn core::future::Future<Output = spark_core::Result<usize, CoreError>> + Send + 'ctx>,
    >
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    type WriteFuture<'ctx>
        = Pin<
        Box<dyn core::future::Future<Output = spark_core::Result<usize, CoreError>> + Send + 'ctx>,
    >
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    type ShutdownFuture<'ctx>
        =
        Pin<Box<dyn core::future::Future<Output = spark_core::Result<(), CoreError>> + Send + 'ctx>>
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    type FlushFuture<'ctx>
        =
        Pin<Box<dyn core::future::Future<Output = spark_core::Result<(), CoreError>> + Send + 'ctx>>
    where
        Self: 'ctx,
        Self::CallCtx<'ctx>: 'ctx;

    fn id(&self) -> Cow<'_, str> {
        Cow::Owned(format!(
            "tls:{}->{}",
            self.inner.local_addr, self.inner.peer_addr
        ))
    }

    fn peer_addr(&self) -> Option<TransportSocketAddr> {
        Some(self.inner.peer_addr)
    }

    fn local_addr(&self) -> Option<TransportSocketAddr> {
        Some(self.inner.local_addr)
    }

    fn read<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        buf: &'ctx mut [u8],
    ) -> Self::ReadFuture<'ctx> {
        Box::pin(async move { TlsChannel::read(self, ctx, buf).await })
    }

    fn write<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        buf: &'ctx [u8],
    ) -> Self::WriteFuture<'ctx> {
        Box::pin(async move { TlsChannel::write(self, ctx, buf).await })
    }

    fn flush<'ctx>(&'ctx self, ctx: &'ctx Self::CallCtx<'ctx>) -> Self::FlushFuture<'ctx> {
        Box::pin(async move { TlsChannel::flush(self, ctx).await })
    }

    fn shutdown<'ctx>(
        &'ctx self,
        ctx: &'ctx Self::CallCtx<'ctx>,
        direction: ShutdownDirection,
    ) -> Self::ShutdownFuture<'ctx> {
        Box::pin(async move {
            match direction {
                ShutdownDirection::Write | ShutdownDirection::Both => self.shutdown(ctx).await,
                ShutdownDirection::Read => Ok(()),
            }
        })
    }

    fn classify_backpressure(
        &self,
        _ctx: &Self::ReadyCtx<'_>,
        _metrics: &BackpressureMetrics,
    ) -> BackpressureDecision {
        BackpressureDecision::Ready
    }
}

#[allow(dead_code)]
fn _assert_tls_transport_connection()
where
    TlsChannel: TransportConnectionTrait<Error = CoreError>,
{
}
